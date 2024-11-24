use std::{
    fs::{self, create_dir, remove_file, File},
    io::{BufRead as _, BufReader},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    str::FromStr,
    thread::panicking,
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _};
use glob::glob;
#[allow(unused_imports)]
use log::{debug, info};
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
    time::{sleep, timeout},
};

async fn wait_for<F>(mut predicate: F, timeout_dur: Duration) -> anyhow::Result<()>
where
    F: FnMut() -> anyhow::Result<bool>,
{
    timeout(timeout_dur, async {
        loop {
            match predicate() {
                Ok(true) => break Ok(()),
                Ok(false) => sleep(Duration::from_millis(100)).await,
                Err(err) => break Err(anyhow!("timeout predicate failed: {}", err)),
            }
        }
    })
    .await
    .context(format!("timeout after {:?}", timeout_dur))?
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

struct LimmatChildBuilder {
    temp_dir: TempDir,
    db_dir: PathBuf,
}

impl LimmatChildBuilder {
    async fn new() -> anyhow::Result<Self> {
        let temp_dir = TempDir::with_prefix("limmat-child")?;
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

        let db_dir = temp_dir.path().join("cache");
        create_dir(&db_dir).unwrap();
        Ok(Self { temp_dir, db_dir })
    }

    fn db_dir(mut self, dir: PathBuf) -> Self {
        self.db_dir = dir;
        self
    }

    async fn start(
        self,
        config: String,
        args: impl IntoIterator<Item = &str>,
    ) -> anyhow::Result<LimmatChild> {
        let worktree_dir = self.temp_dir.path().join("worktrees");
        create_dir(&worktree_dir).unwrap();

        let stderr = File::create(self.temp_dir.path().join("stderr.txt"))?;

        let mut cmd: Command = get_test_bin("limmat").into();
        let cmd = cmd
            .args([
                "--config",
                "/dev/stdin",
                "--repo",
                self.temp_dir.path().to_str().unwrap(),
                "--result-db",
                self.db_dir.to_str().unwrap(),
                "--worktree-dir",
                worktree_dir.to_str().unwrap(),
                "--worktree-prefix",
                "test-worktree-",
            ])
            .args(args)
            .stdin(Stdio::piped())
            .stderr(stderr)
            .stdout(Stdio::null())
            .env("RUST_LOG", "debug")
            .kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        stdin.write_all(config.as_bytes()).await.unwrap();
        Ok(LimmatChild {
            temp_dir: self.temp_dir,
            child,
        })
    }
}

// An instance of the binary, running as a child process.
struct LimmatChild {
    temp_dir: TempDir,
    child: Child,
}

impl Drop for LimmatChild {
    fn drop(&mut self) {
        if panicking() {
            // Hack: when running tests via cargo-stress, there's no convenient
            // way to get stderr of the test process, let alone of its
            // subprocesses. So just dump the child's stderr to stdout when
            // we're panicking.
            let file = match File::open(self.temp_dir.path().join("stderr.txt")) {
                // Don't panic while panicking, it's messy
                Err(err) => {
                    eprintln!("Failed to open stderr file while panicking: {}", err);
                    return;
                }
                Ok(file) => file,
            };
            eprintln!("Panic happening, assuming its a test failure, dumping child stderr.");
            let reader = BufReader::new(file);
            // Read the file line-by-line and print each line
            for line in reader.lines() {
                eprintln!(
                    "{}",
                    line.unwrap_or_else(|e| format!("<read error: {}>", e))
                );
            }
        }
    }
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

impl LimmatChild {
    // Returns true if any worktree of this child currently exists.
    fn has_worktrees(&mut self) -> anyhow::Result<bool> {
        let mut pattern = self.temp_dir.path().join("worktrees").to_owned();
        pattern.push("test-worktree-*");
        Ok(!glob(pattern.to_string_lossy().as_ref())?
            .collect::<Vec<_>>()
            .is_empty())
    }

    async fn terminate(&mut self) -> anyhow::Result<()> {
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
        .await
    }
}

#[test_case("echo hello world"; "clean worktree")]
#[test_case("echo hello world > file.txt"; "dirty worktree")]
#[test_case("echo hello world > file.txt && git add file.txt"; "really dirty worktree")]
#[tokio::test]
async fn test_worktree_teardown(test_command: &str) {
    let mut limmat = LimmatChildBuilder::new()
        .await
        .unwrap()
        .start(
            format!(
                r##"
                num_worktrees = 1
                [[tests]]
                name = "my_test"
                command = {test_command:?}
            "##
            ),
            ["watch", "HEAD^"],
        )
        .await
        .unwrap();

    wait_for(|| limmat.has_worktrees(), Duration::from_secs(5))
        .await
        .expect("worktree not found after 5s");

    limmat.terminate().await.expect("couldn't shut down child");

    assert!(
        !limmat.has_worktrees().unwrap(),
        "worktrees not cleaned up on SIGINT"
    );
}

fn pid_running(pid: pid_t) -> bool {
    return Path::new(&format!("/proc/{pid}")).exists();
}

#[test_log::test(tokio::test)]
async fn shouldnt_leak_jobs() {
    let temp_dir = TempDir::new().unwrap();

    // This config has a test that does not respect SIGTERM. We should not leak
    // that job.
    let mut limmat = LimmatChildBuilder::new()
        .await
        .unwrap()
        .start(
            format!(
                r##"
                num_worktrees = 1
                [[tests]]
                name = "my_test"
                command = "echo $$ > {}/test_pid; while true; do sleep infinity; done"
                shutdown_grace_period_s = 1"##,
                temp_dir.path().to_string_lossy()
            ),
            ["watch", "HEAD^"],
        )
        .await
        .unwrap();

    // Wait for test to start up
    let test_pid_path = temp_dir.path().join("test_pid");
    wait_for(|| Ok(test_pid_path.exists()), Duration::from_secs(5))
        .await
        .expect("worktree not found after 5s");
    let pid: pid_t = pid_t::from_str(fs::read_to_string(test_pid_path).unwrap().trim()).unwrap();

    limmat.terminate().await.unwrap();
    assert!(!pid_running(pid));
}

#[test_log::test(tokio::test)]
async fn should_invalidate_cache_when_dep_changes() {
    let temp_dir = TempDir::new().unwrap();
    let db_dir = temp_dir.path().join("results");
    create_dir(&db_dir).unwrap();

    let test_ran_path = temp_dir.path().join("test_ran");
    {
        let _limmat = LimmatChildBuilder::new()
            .await
            .unwrap()
            .db_dir(db_dir.clone())
            .start(
                format!(
                    r##"
                        num_worktrees = 1
                        [[tests]]
                        name = "my_dependency"
                        command = "echo jello verld"
                        [[tests]]
                        name = "my_test"
                        command = "echo bello burld > {}"
                        depends_on = ["my_dependency"]
                    "##,
                    test_ran_path.as_os_str().to_string_lossy(),
                ),
                ["watch", "HEAD^"],
            )
            .await
            .unwrap();
        wait_for(|| Ok(test_ran_path.exists()), Duration::from_secs(5))
            .await
            .expect("test not ran");

        // Shut down child by dropping it. This is racy, it's possible we
        // haven't finished writing the test DB yet. In that case this test
        // could pass when it should fail, but it shouldn't make the test fail
        // when it should pass.
    }

    // Now we'll run it again but with a different config for the dependency.
    // The dependee should get run again even though its config hasn't changed.
    remove_file(&test_ran_path).unwrap();
    let _limmat = LimmatChildBuilder::new()
        .await
        .unwrap()
        .db_dir(db_dir)
        .start(
            format!(
                r##"
                    num_worktrees = 1
                    [[tests]]
                    name = "my_dependency"
                    command = "echo its all ogre now"
                    [[tests]]
                    name = "my_test"
                    command = "echo bello burld > {}"
                    depends_on = ["my_dependency"]
                "##,
                test_ran_path.as_os_str().to_string_lossy(),
            ),
            ["watch", "HEAD^"],
        )
        .await
        .unwrap();
    wait_for(|| Ok(test_ran_path.exists()), Duration::from_secs(5))
        .await
        .expect("test not re-ran when dependency config changed");
}
