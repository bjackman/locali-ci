use std::{
    fs::{self, create_dir},
    path::Path,
    process::{ExitStatus, Stdio},
    str::FromStr,
    time::{Duration, Instant},
};

use anyhow::{bail, Context as _};
use glob::glob;
use nix::{
    libc::pid_t,
    sys::signal::{kill, Signal},
    unistd::Pid,
};
use tempfile::TempDir;
use test_bin::get_test_bin;
use test_case::test_case;
use tokio::{
    io::AsyncWriteExt as _,
    process::{Child, Command},
};

fn wait_for<F>(mut predicate: F, timeout: Duration) -> anyhow::Result<()>
where
    F: FnMut() -> anyhow::Result<bool>,
{
    let start = Instant::now();
    while start.elapsed() < timeout {
        if predicate().context("timeout predicate failed")? {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timeout after {:?}", timeout)
}

// Add a reasonable method for sending general signals, Rust only provides a method to SIGKILL.
pub trait ChildExt {
    fn signal(&self, sig: Signal) -> anyhow::Result<()>;
}

impl ChildExt for Child {
    fn signal(&self, sig: Signal) -> anyhow::Result<()> {
        let pid = Pid::from_raw(
            self.id()
                .context("no PID for child")?
                .try_into()
                .context("couldnt parse child PID")?,
        );
        kill(pid, sig).context("couldn't signal child")
    }
}

// An instance of the binary, running as a child process.
struct LocalCiChild {
    temp_dir: TempDir,
    child: Child,
}

trait ExitStatusExt {
    fn check_exit_ok(&self) -> anyhow::Result<()>;
}

impl ExitStatusExt for ExitStatus {
    fn check_exit_ok(&self) -> anyhow::Result<()> {
        if self.success() {
            Ok(())
        } else {
            bail!("command failed: {self:?}")
        }
    }
}

impl LocalCiChild {
    async fn new(config: String) -> anyhow::Result<Self> {
        let temp_dir = TempDir::new()?;

        Command::new("git")
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .arg("init")
            .current_dir(temp_dir.path())
            .status()
            .await?
            .check_exit_ok()
            .context("git init")?;
        for _ in 0..5 {
            Command::new("git")
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .current_dir(temp_dir.path())
                .args(["commit", "--allow-empty", "-m", "lohs geht's buebe"])
                .status()
                .await?
                .check_exit_ok()
                .context("git commit")?;
        }

        let worktree_dir = temp_dir.path().join("worktrees");
        create_dir(&worktree_dir).unwrap();
        let cache_dir = temp_dir.path().join("cache");
        create_dir(&cache_dir).unwrap();

        let mut cmd: Command = get_test_bin("local-ci").into();
        let cmd = cmd
            .args([
                "HEAD^",
                "--config",
                "/dev/stdin",
                "--worktree-dir",
                worktree_dir.to_str().unwrap(),
                "--worktree-prefix",
                "test-worktree-",
                "--repo",
                temp_dir.path().to_str().unwrap(),
                "--result-db",
                cache_dir.to_str().unwrap(),
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        stdin.write_all(config.as_bytes()).await.unwrap();
        Ok(Self { temp_dir, child })
    }

    // Returns true if any worktree of this child currently exists.
    fn has_worktrees(&mut self) -> anyhow::Result<bool> {
        let mut pattern = self.temp_dir.path().join("worktrees").to_owned();
        pattern.push("test-worktree-*");
        Ok(!glob(pattern.to_string_lossy().as_ref())?
            .collect::<Vec<_>>()
            .is_empty())
    }

    fn terminate(&mut self) -> anyhow::Result<()> {
        self.child.signal(Signal::SIGINT).unwrap();
        wait_for(
            || {
                match self
                    .child
                    .try_wait()
                    .context("couldn't check child status")?
                {
                    None => Ok(false), // Still running
                    Some(exit_status) => {
                        if exit_status.success() {
                            Ok(true)
                        } else {
                            bail!("test binary failed ({exit_status:?})")
                        }
                    }
                }
            },
            Duration::from_secs(5),
        )
    }
}

#[test_case("echo hello world"; "clean worktree")]
#[test_case("echo hello world > file.txt"; "dirty worktree")]
#[test_case("echo hello world > file.txt && git add file.txt"; "really dirty worktree")]
#[tokio::test]
async fn test_worktree_teardown(test_command: &str) {
    let mut lci = LocalCiChild::new(format!(
        r##"
        num_worktrees = 1
        [[tests]]
        name = "my_test"
        command = {test_command:?}
    "##
    ))
    .await
    .unwrap();

    wait_for(|| lci.has_worktrees(), Duration::from_secs(5)).expect("worktree not found after 5s");

    lci.terminate().expect("couldn't shut down child");

    assert!(
        !lci.has_worktrees().unwrap(),
        "worktrees not cleaned up on SIGINT"
    );
}

fn pid_running(pid: pid_t) -> bool {
    return Path::new(&format!("/proc/{pid}")).exists();
}

#[test_log::test(tokio::test)]
async fn shouldnt_leak_jobs() {
    let temp_dir = TempDir::new().unwrap();

    // This config has a test that does not respect SIGINT. We should not leak
    // that job.
    let mut lci = LocalCiChild::new(format!(
        r##"
        num_worktrees = 1
        [[tests]]
        name = "my_test"
        command = "echo $$ > {}/test_pid; while true; do sleep infinity; done"
        shutdown_grace_period_s = 1
    "##,
        temp_dir.path().to_string_lossy()
    ))
    .await
    .unwrap();

    // Wait for test to start up
    let test_pid_path = temp_dir.path().join("test_pid");
    wait_for(|| Ok(test_pid_path.exists()), Duration::from_secs(5))
        .expect("worktree not found after 5s");
    let pid: pid_t = pid_t::from_str(fs::read_to_string(test_pid_path).unwrap().trim()).unwrap();

    lci.terminate().unwrap();
    assert!(!pid_running(pid));
}
