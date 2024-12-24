use std::{
    collections::HashMap,
    ffi::{OsStr, OsString},
    fs::{self, create_dir, create_dir_all, remove_file, File},
    io::{BufRead as _, BufReader},
    os::unix::process::ExitStatusExt as _,
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    result,
    str::FromStr,
    thread::panicking,
    time::Duration,
};

use anyhow::{anyhow, bail, Context as _};
use glob::{glob, GlobError};
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
    env: HashMap<OsString, OsString>,
    config: String,
}

impl<'a> LimmatChildBuilder {
    async fn new(config: impl AsRef<str>) -> anyhow::Result<Self> {
        let temp_dir = TempDir::with_prefix("limmat-child")?;

        let db_dir = temp_dir.path().join("cache");
        create_dir(&db_dir).unwrap();
        Ok(Self {
            temp_dir,
            existing_repo_dir: None,
            db_dir,
            dump_output_on_panic: true,
            env: HashMap::from([("RUST_LOG".into(), "debug".into())]),
            config: config.as_ref().to_string(),
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

    fn env(mut self, k: &str, v: &OsStr) -> Self {
        self.env.insert(k.into(), v.to_owned());
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

    async fn start(&self, args: impl IntoIterator<Item = &str>) -> anyhow::Result<LimmatChild> {
        let worktree_dir = self.temp_dir.path().join("worktrees");
        create_dir_all(&worktree_dir).unwrap();

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
            .envs(&self.env)
            .kill_on_drop(true);
        let mut child = cmd.spawn().unwrap();
        let mut stdin = child.stdin.take().unwrap();

        stdin.write_all(self.config.as_bytes()).await.unwrap();
        Ok(LimmatChild {
            builder: self,
            child,
            dump_output_on_panic: self.dump_output_on_panic,
        })
    }
}

// An instance of the binary, running as a child process.
struct LimmatChild<'a> {
    builder: &'a LimmatChildBuilder,
    child: Child,
    dump_output_on_panic: bool,
}

impl<'a> LimmatChild<'a> {
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
        fs::read_to_string(self.builder.temp_dir.path().join("stdout.txt"))
            .context("reading child stdout")
    }

    fn dump_stderr(&self) {
        let file = match File::open(self.builder.temp_dir.path().join("stderr.txt")) {
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

impl Drop for LimmatChild<'_> {
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

impl<'a> LimmatChild<'a> {
    // Returns true if any worktree of this child currently exists.
    fn has_worktrees(&mut self) -> anyhow::Result<bool> {
        let mut pattern = self.builder.temp_dir.path().join("worktrees").to_owned();
        pattern.push("test-worktree-*");
        Ok(!glob(pattern.to_string_lossy().as_ref())?
            .collect::<Vec<_>>()
            .is_empty())
    }

    // Blocks until a result for the given test and revision exists, by running
    // the "get" command repeatedly.
    async fn result_exists(&self, test: &str, rev: &str) -> anyhow::Result<()> {
        loop {
            let mut child = self.builder.start(["get", test, rev]).await?;
            let status = child
                .child
                .wait()
                .await
                .context("error waiting for child")?;
            match status.code() {
                None => bail!("get command terminated by signal {:?}", status.signal()),
                Some(50) => continue, // Result not found.
                Some(0) => return Ok(()),
                Some(exit_code) => {
                    eprintln!("Dumping failed 'get' command stderr...");
                    child.dump_stderr();
                    bail!("get command failed with code {exit_code}")
                }
            }
        }
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
    let builder = LimmatChildBuilder::new(format!(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = {test_command:?}
        "##
    ))
    .await
    .unwrap();
    let mut limmat = builder.start(["watch", "HEAD^"]).await.unwrap();

    timeout(
        Duration::from_secs(20),
        limmat.result_exists("my_test", "HEAD"),
    )
    .await
    .expect("result not found after 20s")
    .expect("failed to check for test result");

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
    let builder = LimmatChildBuilder::new(format!(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = "echo $$ > {}/test_pid; while true; do sleep infinity; done"
            shutdown_grace_period_s = 1"##,
        temp_dir.path().to_string_lossy()
    ))
    .await
    .unwrap();
    let mut limmat = builder.start(["watch", "HEAD^"]).await.unwrap();

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
        let builder = LimmatChildBuilder::new(format!(
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
        ))
        .await
        .unwrap()
        .db_dir(db_dir.clone());
        let _limmat = builder.start(["watch", "HEAD^"]).await.unwrap();
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
    let builder = LimmatChildBuilder::new(format!(
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
    ))
    .await
    .unwrap()
    .db_dir(db_dir);
    let _limmat = builder.start(["watch", "HEAD^"]).await.unwrap();
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
    let builder = LimmatChildBuilder::new(config).await.unwrap();
    let mut child = builder.start(["test", "my_test"]).await.unwrap();
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
    let builder = LimmatChildBuilder::new(&config)
        .await
        .unwrap()
        .db_dir(db_dir.path().to_owned())
        .existing_repo_dir(repo_dir.path().to_owned());
    let mut child = builder.start(["test", "my_other_dep"]).await.unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(child.stdout().unwrap(), eq("ill be your friend\n"));

    assert_that!(
        fs::read_to_string(&signalling_path),
        ok(eq("ill help you carry on\n"))
    );

    // Now run the actual test, reusing the result DB and the same builder
    let mut child = builder.start(["test", "my_test"]).await.unwrap();
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

    let db_dir = TempDir::with_prefix("result-db").unwrap();
    let builder = LimmatChildBuilder::new(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = """
            echo burgle schmurgle
            echo bungle bingle >&2
            touch $LIMMAT_ARTIFACTS/my_artifact
            """

            shutdown_grace_period_s = 1
        "##,
    )
    .await
    .unwrap()
    .db_dir(db_dir.path().to_owned())
    .existing_repo_dir(repo_dir.path().to_owned());
    let mut child = builder
        .start(["get", "--run", "my_test", "HEAD^"])
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

    let mut child = builder
        .start(["get", "my_test", "HEAD^", "stderr"])
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

    let mut child = builder
        .start(["artifacts", "my_test", "HEAD^"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_true!(Path::new(child.stdout().unwrap().trim())
        .join("my_artifact")
        .exists());
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

    let builder = LimmatChildBuilder::new(format!(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = "touch {}/started.$$.$LIMMAT_COMMIT && sleep 1"
            shutdown_grace_period_s = 1
        "##,
        tmp_dir.path().display()
    ))
    .await
    .unwrap()
    .db_dir(db_dir.path().to_owned())
    // Hack - this is making for annoying log spam when it fails at the moment.
    .dump_output_on_panic(false)
    .existing_repo_dir(repo_dir.path().to_owned());
    let mut watch_children = Vec::new();
    for _ in 0..16 {
        let child = builder.start(["watch", "HEAD^"]).await.unwrap();
        watch_children.push(child);
    }
    let mut get_children: Vec<LimmatChild> = Vec::new();
    for _ in 0..16 {
        let child = builder
            .start(["get", "--run", "my_test", "HEAD"])
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

#[googletest::test]
#[tokio::test]
async fn limmat_artifacts_test_cmd() {
    let temp_dir = TempDir::new().unwrap();

    let builder = LimmatChildBuilder::new(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = "echo hwat >> $LIMMAT_ARTIFACTS/foo"
        "##,
    )
    .await
    .unwrap()
    .env("TMPDIR", temp_dir.path().as_os_str());
    let mut child = builder.start(["test", "my_test"]).await.unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();

    // The "test" command dumps artifacts into a temp directory, there should be
    // a single file in the temp directory (in a subdir) which should be a
    // single file.
    let mut artifacts_dir_files: Vec<PathBuf> = glob(
        &temp_dir
            .path()
            .join("**")
            .join("artifacts")
            .join("*")
            .to_string_lossy(),
    )
    .expect("failed to glob")
    .collect::<result::Result<Vec<PathBuf>, GlobError>>()
    .expect("error in glob result")
    .into_iter()
    .filter(|p| !p.is_dir())
    .collect();
    assert_that!(artifacts_dir_files, len(eq(1)));
    let artifact_path = artifacts_dir_files.pop().unwrap();
    expect_that!(artifact_path.file_name(), some(eq("foo")));
    expect_that!(
        artifact_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent()),
        some(eq(temp_dir.path()))
    );
    expect_that!(fs::read_to_string(artifact_path), ok(eq("hwat\n")));
}

#[googletest::test]
#[tokio::test]
async fn artifacts_cmd() {
    let temp_dir = TempDir::new().unwrap();

    let builder = LimmatChildBuilder::new(
        r##"
            num_worktrees = 1
            [[tests]]
            name = "my_test"
            command = "echo hwat >> $LIMMAT_ARTIFACTS/foo"
        "##,
    )
    .await
    .unwrap()
    .env("TMPDIR", temp_dir.path().as_os_str());
    let mut child = builder.start(["test", "my_test"]).await.unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();

    // The "test" command dumps artifacts into a temp directory, there should be
    // a single file in the temp directory (in a subdir) which should be a
    // single file.
    let mut artifacts_files: Vec<PathBuf> = glob(
        &temp_dir
            .path()
            .join("**")
            .join("artifacts")
            .join("*")
            .to_string_lossy(),
    )
    .expect("failed to glob")
    .collect::<result::Result<Vec<PathBuf>, GlobError>>()
    .expect("error in glob result")
    .into_iter()
    .filter(|p| !p.is_dir())
    .collect();
    assert_that!(artifacts_files, len(eq(1)));
    let artifact_path = artifacts_files.pop().unwrap();
    expect_that!(artifact_path.file_name(), some(eq("foo")));
    expect_that!(
        artifact_path
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent()),
        some(eq(temp_dir.path()))
    );
    expect_that!(fs::read_to_string(artifact_path), ok(eq("hwat\n")));
}
static DEP_CONFIG: &str = r##"
    num_worktrees = 1

    [[tests]]
    name = "dep"
    command = "echo 'ye mighty' >> $LIMMAT_ARTIFACTS/ozy"

    [[tests]]
    name = "dep2"
    command = "echo 'and dispair' >> $LIMMAT_ARTIFACTS/trunkless"

    [[tests]]
    name = "main"
    depends_on = ["dep", "dep2"]
    command = "cat $LIMMAT_ARTIFACTS_dep/ozy $LIMMAT_ARTIFACTS_dep2/trunkless"
"##;

#[googletest::test]
#[tokio::test]
async fn dependency_artifacts_get_cmd() {
    let builder = LimmatChildBuilder::new(DEP_CONFIG.to_string())
        .await
        .unwrap();
    let mut child = builder
        .start(["get", "--run", "main", "HEAD^", "stdout"])
        .await
        .unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(
        fs::read_to_string(child.stdout().unwrap().trim()),
        ok(eq("ye mighty\nand dispair\n"))
    );
}

#[googletest::test]
#[tokio::test]
async fn dependency_artifacts_test_cmd() {
    let builder = LimmatChildBuilder::new(DEP_CONFIG.to_string())
        .await
        .unwrap();
    let mut child = builder.start(["test", "main"]).await.unwrap();
    timeout(Duration::from_secs(5), child.expect_success())
        .await
        .expect("child didn't shut down")
        .unwrap();
    expect_that!(child.stdout().unwrap(), eq("ye mighty\nand dispair\n"));
}
