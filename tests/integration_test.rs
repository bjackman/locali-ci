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
use googletest::{expect_that, prelude::*};
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
    existing_repo_dir: Option<PathBuf>, // If None, create one right in temp_dir.
    db_dir: PathBuf,
    dump_output_on_panic: bool,
}

impl LimmatChildBuilder {
    async fn new() -> anyhow::Result<Self> {
        let temp_dir = TempDir::with_prefix("limmat-child")?;

        let db_dir = temp_dir.path().join("cache");
        create_dir(&db_dir).unwrap();
        Ok(Self {
            temp_dir,
            existing_repo_dir: None,
            db_dir,
            dump_output_on_panic: true,
        })
    }

    fn db_dir(mut self, dir: PathBuf) -> Self {
        self.db_dir = dir;
        self
    }

    fn existing_repo_dir(mut self, dir: PathBuf) -> Self {
        self.existing_repo_dir = Some(dir);
        self
    }

    fn dump_output_on_panic(mut self, dump: bool) -> Self {
        self.dump_output_on_panic = dump;
        self
    }

    async fn init_test_repo(path: &Path) -> anyhow::Result<()> {
        Command::new("git")
            .stderr(Stdio::null())
            .stdout(Stdio::null())
            .arg("init")
            .current_dir(path)
            .status()
            .await?
            .check_exit_ok()
            .context("git init")?;
        for _ in 0..5 {
            Command::new("git")
                .stderr(Stdio::null())
                .stdout(Stdio::null())
                .current_dir(path)
                .args(["commit", "--allow-empty", "-m", "lohs geht's buebe"])
                .status()
                .await?
                .check_exit_ok()
                .context("git commit")?;
        }
        Ok(())
    }

    async fn start(
        self,
        config: impl AsRef<str>,
        args: impl IntoIterator<Item = &str>,
    ) -> anyhow::Result<LimmatChild> {
        let worktree_dir = self.temp_dir.path().join("worktrees");
        create_dir(&worktree_dir).unwrap();

        let stderr = File::create(self.temp_dir.path().join("stderr.txt"))?;
        let stdout = File::create(self.temp_dir.path().join("stdout.txt"))?;

        let repo_dir = match self.existing_repo_dir {
            Some(ref dir) => dir,
            None => {
                Self::init_test_repo(self.temp_dir.path()).await?;
                self.temp_dir.path()
            }
        };

        let mut cmd: Command = get_test_bin("limmat").into();
        let cmd = cmd
            .args([
                "--config",
                "/dev/stdin",
                "--repo",
                repo_dir.to_str().unwrap(),
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
            .stdout(stdout)
            .env("RUST_LOG", "debug")
            .kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        stdin.write_all(config.as_ref().as_bytes()).await.unwrap();
        Ok(LimmatChild {
            temp_dir: self.temp_dir,
            child,
            dump_output_on_panic: self.dump_output_on_panic,
        })
    }
}

// An instance of the binary, running as a child process.
struct LimmatChild {
    temp_dir: TempDir,
    child: Child,
    dump_output_on_panic: bool,
}

impl LimmatChild {
    // Block until the process has terminated and return an error if isn't successful.
    async fn expect_success(&mut self) -> anyhow::Result<()> {
        let status = self.child.wait().await.expect("error waiting for child");
        if !status.success() {
            if !self.dump_output_on_panic {
                eprintln!("Child Limmat process failed, dumping stderr");
                self.dump_stderr();
            }
            bail!("Child process didn't succed: {:?}", status);
        }
        Ok(())
    }

    fn stdout(&self) -> anyhow::Result<String> {
        fs::read_to_string(self.temp_dir.path().join("stdout.txt")).context("reading child stdout")
    }

    fn dump_stderr(&self) {
        let file = match File::open(self.temp_dir.path().join("stderr.txt")) {
            // Don't panic while panicking, it's messy
            Err(err) => {
                eprintln!("Failed to open stderr file while panicking: {}", err);
                return;
            }
            Ok(file) => file,
        };
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

impl Drop for LimmatChild {
    fn drop(&mut self) {
        // Hack: when running tests via cargo-stress, there's no convenient
        // way to get stderr of the test process, let alone of its
        // subprocesses. So just dump the child's stderr to stdout when
        // we're panicking.
        if self.dump_output_on_panic && panicking() {
            eprintln!("Panic happening, assuming its a test failure, dumping child stderr.");
            self.dump_stderr();
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
    Path::new(&format!("/proc/{pid}")).exists()
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

#[test_case(
    r##"
        num_worktrees = 1
        [[tests]]
        name = "my_test"
        command = "echo burgle schmurgle"
        shutdown_grace_period_s = 1
    "##;
    "smoke")]
#[test_case(
    r##"
        num_worktrees = 1
        [[tests]]
        name = "my_dep"
        command = "echo lean on me"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_test"
        depends_on = ["my_dep"]
        command = "echo burgle schmurgle"
        shutdown_grace_period_s = 1
    "##;
    "one_dep")]
#[test_case(
    r##"
        num_worktrees = 1
        [[tests]]
        name = "nobody_cares"
        command = "echo so lonely"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_dep"
        command = "echo when youre not strong"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_transitive_dep"
        command = "echo ill help you carry one"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_other_dep"
        depends_on = ["my_transitive_dep"]
        command = "echo ill be your friend"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_test"
        depends_on = ["my_dep", "my_other_dep"]
        command = "echo burgle schmurgle"
        shutdown_grace_period_s = 1
    "##;
    "loads")]
#[googletest::test]
#[tokio::test]
async fn should_run_test(config: &str) {
    let mut child = LimmatChildBuilder::new()
        .await
        .unwrap()
        .start(config, ["test", "my_test"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(child.stdout().unwrap(), eq("burgle schmurgle\n"));
}

#[googletest::test]
#[tokio::test]
async fn should_run_test_with_stored_results() {
    let repo_dir = TempDir::with_prefix("repo").unwrap();
    LimmatChildBuilder::init_test_repo(repo_dir.path())
        .await
        .unwrap();
    let temp_dir = TempDir::new().unwrap();

    let signalling_path = temp_dir.path().join("trans-output.txt");
    let config = format!(
        r##"
        num_worktrees = 1
        [[tests]]
        name = "nobody_cares"
        command = "echo so lonely"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_dep"
        command = "echo when youre not strong"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_transitive_dep"
        command = "echo ill help you carry on >> {}"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_other_dep"
        depends_on = ["my_transitive_dep"]
        command = "echo ill be your friend"
        shutdown_grace_period_s = 1
        [[tests]]
        name = "my_test"
        depends_on = ["my_dep", "my_other_dep"]
        command = "echo burgle schmurgle"
        shutdown_grace_period_s = 1
    "##,
        signalling_path.display()
    );

    // Awkward hack to get a result into the database: we we run my_dep (whose
    // result won't get cached) to get my_transitive_dep into the cache.
    let db_dir = TempDir::with_prefix("result-db").unwrap();
    let mut child = LimmatChildBuilder::new()
        .await
        .unwrap()
        .db_dir(db_dir.path().to_owned())
        .existing_repo_dir(repo_dir.path().to_owned())
        .start(&config, ["test", "my_other_dep"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(child.stdout().unwrap(), eq("ill be your friend\n"));

    assert_that!(
        fs::read_to_string(&signalling_path),
        ok(eq("ill help you carry on\n"))
    );

    // Now run the actual test, reusing the result DB.
    let mut child = LimmatChildBuilder::new()
        .await
        .unwrap()
        .db_dir(db_dir.path().to_owned())
        .existing_repo_dir(repo_dir.path().to_owned())
        .start(config, ["test", "my_test"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(child.stdout().unwrap(), eq("burgle schmurgle\n"));

    // If this fails then probably it means the result of the dependency job was not cached.
    // But it could also be something weirder.
    assert_that!(
        fs::read_to_string(&signalling_path),
        ok(eq("ill help you carry on\n")),
    );
}

#[test_case("burgle schmurgle\n", "bungle bingle\n" ; "no merge")]
#[googletest::test]
#[tokio::test]
async fn should_find_output(want_stdout: &str, want_stderr: &str) {
    let repo_dir = TempDir::with_prefix("repo").unwrap();
    LimmatChildBuilder::init_test_repo(repo_dir.path())
        .await
        .unwrap();

    let config = r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = """
            echo burgle schmurgle
            echo bungle bingle >&2
            """

            shutdown_grace_period_s = 1
        "##;
    let db_dir = TempDir::with_prefix("result-db").unwrap();
    let mut child = LimmatChildBuilder::new()
        .await
        .unwrap()
        .db_dir(db_dir.path().to_owned())
        .existing_repo_dir(repo_dir.path().to_owned())
        .start(&config, ["get", "--run", "my_test", "HEAD^"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(
        fs::read_to_string(child.stdout().unwrap().trim()),
        ok(eq(want_stdout))
    );

    let mut child = LimmatChildBuilder::new()
        .await
        .unwrap()
        .db_dir(db_dir.path().to_owned())
        .existing_repo_dir(repo_dir.path().to_owned())
        .start(config, ["get", "my_test", "HEAD^", "stderr"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(
        fs::read_to_string(child.stdout().unwrap().trim()),
        ok(eq(want_stderr))
    );
}

#[googletest::test]
#[tokio::test]
async fn should_find_not_race() {
    let repo_dir = TempDir::with_prefix("repo").unwrap();
    let tmp_dir = TempDir::new().unwrap();
    LimmatChildBuilder::init_test_repo(repo_dir.path())
        .await
        .unwrap();
    let db_dir = TempDir::with_prefix("result-db").unwrap();

    let config = format!(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = "touch {}/started.$$.$LIMMAT_COMMIT && sleep 1"
            shutdown_grace_period_s = 1
        "##,
        tmp_dir.path().display()
    );
    let mut watch_children = Vec::new();
    for _ in 0..16 {
        let child = LimmatChildBuilder::new()
            .await
            .unwrap()
            .db_dir(db_dir.path().to_owned())
            // Hack - this is making for annoying log spam when it fails at the moment.
            .dump_output_on_panic(false)
            .existing_repo_dir(repo_dir.path().to_owned())
            .start(&config, ["watch", "HEAD^"])
            .await
            .unwrap();
        watch_children.push(child);
    }
    let mut get_children: Vec<LimmatChild> = Vec::new();
    for _ in 0..16 {
        let child = LimmatChildBuilder::new()
            .await
            .unwrap()
            .db_dir(db_dir.path().to_owned())
            .existing_repo_dir(repo_dir.path().to_owned())
            // Hack - this is making for annoying log spam when it fails at the moment.
            .dump_output_on_panic(false)
            .start(&config, ["get", "--run", "my_test", "HEAD"])
            .await
            .unwrap();
        get_children.push(child);
    }

    for mut child in get_children.into_iter() {
        timeout(Duration::from_secs(5), child.expect_success())
            .await
            .unwrap()
            .unwrap();
    }

    // Even though we ran loads of instances of Limmat, only one of them should
    // have ran the job.
    let pattern = tmp_dir.path().join("started.*").to_owned();
    expect_that!(
        glob(pattern.to_string_lossy().as_ref())
            .unwrap()
            .collect::<Vec<_>>(),
        len(eq(1))
    );
}
