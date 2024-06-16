use core::fmt;
use core::fmt::Display;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use futures::future::{self, try_join_all, Either};
use log::info;
use nix::sys::signal::kill;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

#[cfg(test)]
use crate::git::PersistentWorktree;
use crate::git::TempWorktree;
use crate::git::{CommitHash, Worktree};
use crate::pool::Pool;
use crate::process::OutputExt;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    job_cts: HashMap<CommitHash, CancellationToken>,
    result_tx: broadcast::Sender<Arc<CommitTestResult>>,
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
    worktree_pool: Arc<Pool<TempWorktree>>,
}

impl Manager {
    // Starts the workers. You must call close() before dropping it.
    pub async fn new<W>(
        num_threads: u32,
        // This needs to be an Arc because we hold onto a reference to it for a
        // while, and create temporary worktrees from it in the background.
        repo: Arc<W>,
        program: OsString,
        args: Vec<OsString>,
    ) -> anyhow::Result<Self>
    where
        // We need to specify 'static here. Just because we have an Arc over the
        // repo that doesn't mean it automatically satisfies 'static:
        // https://users.rust-lang.org/t/why-is-t-static-constrained-when-using-arc-t-and-thread-spawn/26262/2
        // It would be much more convenient to just specify some or all these
        // trait bounds as subtraits of Workrtree. But I dunno, that feels Wrong.
        W: Worktree + Sync + Send + 'static,
    {
        let worktrees =
            try_join_all((0..num_threads).map(|_| TempWorktree::create_from::<W>(repo.borrow())))
                .await
                .context("setting up temporary worktrees")?;
        let (result_tx, _) = broadcast::channel(32);
        Ok(Self {
            result_tx,
            job_cts: HashMap::new(),
            program: Arc::new(program),
            args: Arc::new(args),
            worktree_pool: Arc::new(Pool::new(worktrees)),
        })
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    pub fn set_revisions<I: IntoIterator<Item = CommitHash>>(
        &mut self,
        revs: I,
    ) {
        let mut to_start = HashSet::<CommitHash>::from_iter(revs.into_iter());
        let mut cancel_revs = Vec::new();
        for rev in self.job_cts.keys() {
            // We're already testing rev, so we don't need to kick it off below.
            if !to_start.remove(rev) {
                // This rev is being tested but wasn't in rev_set.
                cancel_revs.push(rev.clone())
            }
        }
        info!("Starting {:?}, cancelling {:?}", to_start, cancel_revs);
        for rev in cancel_revs {
            self.job_cts[&rev].cancel();
            self.job_cts.remove(&rev);
        }

        for rev in to_start {
            let ct = CancellationToken::new();
            self.job_cts.insert(rev.to_owned(), ct.clone());
            let test = Test {
                ct,
                program: self.program.clone(),
                args: self.args.clone(),
                rev: rev.to_owned(),
            };
            let pool = self.worktree_pool.clone();
            let tx = self.result_tx.clone();
            tokio::spawn(async move {
                let worktree = pool.get().await;
                let result = test.run(worktree.as_ref()).await;
                tx.send(Arc::new(CommitTestResult {
                    hash: test.rev,
                    result,
                }))
                .expect("couldn't send result");
            });
        }
    }

    // Streams results back. Note you need to call this _before_ you generate the results you want
    // to receive.
    //
    // I think the "proper" solution for this is to return a Stream. But I don't understand it.
    pub fn results(&self) -> broadcast::Receiver<Arc<CommitTestResult>> {
        self.result_tx.subscribe()
    }
}

impl Drop for Manager {
    fn drop(&mut self) {
        self.set_revisions([]);
    }
}

// This is not really a proper type, it doesn't really mean anything except as an implementation
// detail of its user. I tried to get rid of it but then you run into issues with getting references
// to individual fields while a mutable reference exists to the overall struct. I think this is
// basically one an instance of "view structs" described in
// https://smallcultfollowing.com/babysteps/blog/2024/06/02/the-borrow-checker-within/
struct Test {
    ct: CancellationToken,
    // TODO: Unclear if there's a way to avoid the atomic operations incurred by cloning these Arcs.
    // There is no builtin equivalent to thread::scope for async. If we had that, maybe it would
    // become possible to convince the compiler that the Manager outlives its Tests. Not sure.
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
    rev: CommitHash,
}

impl Test {
    async fn run<W>(&self, worktree: &W) -> TestResult
    where
        W: Worktree,
    {
        worktree.checkout(&self.rev).await?;

        let mut cmd = Command::new(self.program.as_ref());
        cmd.args(self.args.as_ref())
            .current_dir(worktree.path());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        let child = cmd.spawn().context("spawning test command")?;
        // lol wat?
        let pid = Pid::from_raw(
            child
                .id()
                .ok_or(anyhow!("no PID for child job"))?
                .try_into()
                .unwrap(),
        );
        // Await the child, or cancellation. Because the "right" branch still needs to do work on
        // the "left" future, tokio::select doesn't grant us any clarity or concision here so we
        // drop down to the raw function call.
        let child_fut = pin!(child.wait_with_output());
        let cancel_fut = pin!(self.ct.cancelled());
        match future::select(child_fut, cancel_fut).await {
            Either::Left((result, _)) =>
            // Test completed, figure out the result. I think maybe a true Rustacean would
            // write this block as a single chain of methods? But it seems ridiculous to me.
            {
                Ok(TestOutcome::Completed {
                    exit_code: result.map_err(anyhow::Error::from)?.code_not_killed()?,
                })
            }
            Either::Right((_, child_fut)) => {
                // Canceled. Shut down the process.
                kill(pid, Signal::SIGINT).context("couldn't interrupt child job")?;
                // We don't care about its result but we
                // need to wait for it to shut down so that we can safely give back the
                // worktree.
                let _ = child_fut.await;

                Ok(TestOutcome::Canceled)
            }
        }
    }
}

#[derive(Debug)]
pub struct CommitTestResult {
    pub hash: CommitHash,
    pub result: TestResult,
}

impl Display for CommitTestResult {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Result: {} => ", self.hash)?;
        match &self.result {
            Ok(outcome) => write!(f, "{}", outcome),
            Err(error) => write!(f, "error running test: {}", error),
        }
    }
}

// There are three results for tests: error (something went wrong when we were trying to run it),
// cancellation, and completion. Ideally we woud just have an enum with three variants, but it's
// really handy for the "error" case to be represented by std::result::Result so that we can use the
// quesiton mark operator. Thus, we have a two-layered result type... Worth it? I dunno...
type TestResult = anyhow::Result<TestOutcome>;

#[derive(Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Canceled,
    Completed { exit_code: i32 },
}

impl Display for TestOutcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Canceled => write!(f, "Cancelled"),
            Self::Completed { exit_code } => write!(f, "Completed - exit code {}", exit_code),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{path::{Path, PathBuf}, thread::panicking, time::Duration};

    use futures::Future;
    use log::error;
    use tempfile::TempDir;
    use test_log;
    use tokio::{
        fs, select,
        time::{interval, sleep},
    };

    use crate::git::{CommitHash, Worktree};

    use super::*;

    // Blocks until file exists, the dumb way, then reads it as a string.
    async fn await_exists_and_read<P>(path: P) -> String
    where
        P: AsRef<Path>,
    {
        let mut interval = interval(Duration::from_millis(10));
        while !path.as_ref().try_exists().unwrap() {
            interval.tick().await;
        }
        fs::read_to_string(path)
            .await
            .expect("couldn't read hash file")
    }

    // TODO: this sucks, find a way to dedupe more. One thing I think we could dedupue is the bash
    // test command - it could be a struct that has methods like called() that blocks until it's
    // started,  and finish() that exits successfully and stuff like that.
    struct Fixture {
        _temp_dir: TempDir,
        repo: Arc<PersistentWorktree>,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp_dir = TempDir::with_prefix("fixture-").expect("couldn't make tempdir");
            let repo = PersistentWorktree::create(temp_dir.path().into())
                .await
                .expect("couldn't init test repo");
            Self {
                _temp_dir: temp_dir,
                repo: Arc::new(repo),
            }
        }
    }

    // A script that can be used as the test command for a Manager, with utilities for testing the
    // manager. The script won't terminate until told to.
    struct TestScript {
        dir: TempDir,
        script: OsString, // Raw content.
    }

    enum Terminate {
        Immediately,
        OnSigint,
    }

    impl TestScript {
        const STARTED_FILENAME_PREFIX: &'static str = "started.";
        const SIGINTED_FILENAME_PREFIX: &'static str = "siginted.";
        const LOCK_FILENAME: &'static str = "lockfile";
        const EXCLUSION_BUG_PATH: &'static str = "exclusion_bug";

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        pub fn new(terminate: Terminate) -> Self {
            let dir = TempDir::with_prefix("test-script-").expect("couldn't make tempdir");
            // The script will touch a special file to notify us that it has been started. On
            // receiving SIGINT it touches a nother special file. Then if Terminate::Never it blocks
            // on input, which it will never receive.
            //
            // The "lockfile" lets us detect if the worktree gets assigned to multiple script
            // instances at once. We would ideally actually do this with flock but it turns out to
            // be a bit of a pain to use, so we just use regular if-statements. I _guess_ we can
            // trust from the PoV of a single thread that this will be consistent, i.e. it cannot
            // produce false positive failures. I am sure that it can produce false negatives, but
            // we could get false negatives here even with flock, since there is always a window
            // between the script starting and it actually taking the lock.
            //
            // Note that the blocking thing (maybe_read) must be a shell builtin; otherwise we would
            // need more Bash hackery to ensure that the signal gets forwarded to it.
            let script = format!(
                "trap \"touch {siginted_path_prefix:?}$(git rev-parse HEAD); exit\" SIGINT
                touch {started_path_prefix:?}$(git rev-parse HEAD)

                if [ -e {lockfile_path:?} ]; then
                    touch {exclusion_bug_path:?}
                fi
                touch {lockfile_path:?}
                trap \"rm {lockfile_path:?}\" EXIT

                {maybe_read}",
                started_path_prefix = dir.path().join(Self::STARTED_FILENAME_PREFIX),
                siginted_path_prefix = dir.path().join(Self::SIGINTED_FILENAME_PREFIX),
                lockfile_path = dir.path().join(Self::LOCK_FILENAME),
                exclusion_bug_path = dir.path().join(Self::EXCLUSION_BUG_PATH),
                maybe_read = match terminate {
                    Terminate::Immediately => "",
                    Terminate::OnSigint => "read",
                }
            );

            Self {
                dir,
                script: script.into(),
            }
        }

        // Pass this to Manager::new
        pub fn program(&self) -> OsString {
            "bash".into()
        }
        // Pass this to Manager::new
        pub fn args(&self) -> Vec<OsString> {
            vec!["-xc".into(), self.script.clone()]
        }

        // Path used by the running script to signal an event.
        fn signalling_path(&self, filename_prefix: &str, hash: &CommitHash) -> PathBuf {
            // Argh I dunno this is annoying.
            let mut filename = OsString::from(filename_prefix);
            filename.push(hash.as_ref());
            self.dir.path().join(filename)
        }

        // If this path exists, two instances of the script used the same worktree at once.
        fn exclusion_bug_path(&self) -> PathBuf {
            self.dir.path().join(Self::EXCLUSION_BUG_PATH)
        }

        // Blocks until the script is started for the given commit hash.
        pub async fn started(&self, hash: &CommitHash) -> StartedTestScript {
            await_exists_and_read(self.signalling_path(Self::STARTED_FILENAME_PREFIX, hash)).await;
            StartedTestScript {
                script: &self,
                hash: hash.to_owned(),
            }
        }
    }

    // Hack to check for stuff that is orthogonal to any particular test, so we
    // don't wanna have to it in every individual test.
    impl Drop for TestScript {
        fn drop(&mut self) {
            if self.exclusion_bug_path().exists() {
                let msg = "Overlapping test script runs used the same worktree";
                if panicking() {
                    // If you panic during a panic (i.e. if this fails when the test had already
                    // failed) you get a huge splat. Just log instead.
                    error!("{}", msg);
                } else {
                    panic!("{}", msg);
                }
            }
        }
    }

    // Like a TestScript, but you can only get one once it's already startd running, so it has extra
    // operations.
    struct StartedTestScript<'a> {
        script: &'a TestScript,
        hash: CommitHash,
    }

    impl<'a> StartedTestScript<'a> {
        // Blocks until the script has received a SIGINT.
        pub async fn siginted(&self) {
            await_exists_and_read(
                self.script
                    .signalling_path(TestScript::SIGINTED_FILENAME_PREFIX, &self.hash),
            )
            .await;
        }
    }

    async fn timeout_1s<F, T>(fut: F) -> anyhow::Result<T>
    where
        F: Future<Output = T>,
    {
        select!(
            _ = sleep(Duration::from_secs(1)) => Err(anyhow!("timeout after 1s")),
            output = fut => Ok(output)
        )
    }

    // anyhow::Error doesn't implement PartialEq. Here's an awkward comparator for
    // CommitTestResults, hopefully good enough for testing...?
    impl PartialEq for CommitTestResult {
        fn eq(&self, other: &Self) -> bool {
            return self.hash == other.hash
                && match (&self.result, &other.result) {
                    (Ok(my_outcome), Ok(other_outcome)) => my_outcome == other_outcome,
                    (Err(my_err), Err(other_err)) => my_err.to_string() == other_err.to_string(),
                    _ => false,
                };
        }
    }

    impl Eq for CommitTestResult {}

    async fn expect_result_1s(
        results: &mut broadcast::Receiver<Arc<CommitTestResult>>,
        want: CommitTestResult,
    ) -> anyhow::Result<()> {
        let result = timeout_1s(results.recv())
            .await
            .context("didn't get result after 1s")?
            .expect("result channel terminated");
        let got = result.as_ref();
        if *got != want {
            return Err(anyhow!(
                "Didn't get expected result - got {} want {}",
                got,
                want
            ));
        }
        Ok(())
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let fixture = Fixture::new().await;
        let hash = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new(Terminate::Immediately);
        let mut m = Manager::new(2, fixture.repo.clone(), script.program(), script.args())
            .await
            .expect("couldn't set up manager");
        let mut results = m.results();
        m.set_revisions(vec![hash.clone()]);
        // TODO: wait until the manager thinks it has no more work to do, using
        // a special "settle" test method.
        // We should get a singular result because we only fed in one revision.
        expect_result_1s(
            &mut results,
            CommitTestResult {
                hash: hash,
                result: Ok(TestOutcome::Completed { exit_code: 0 }),
            },
        )
        .await
        .expect("bad test result");
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let fixture = Fixture::new().await;
        let hash1 = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new(Terminate::OnSigint);
        let mut m = Manager::new(1, fixture.repo.clone(), script.program(), script.args())
            .await
            .expect("couldn't set up manager");
        let mut results = m.results();
        m.set_revisions(vec![hash1.clone()]);
        let started_hash1 = timeout_1s(script.started(&hash1))
            .await
            .expect("script did not run for hash1");
        let hash2 = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        m.set_revisions(vec![hash2.clone()]);
        timeout_1s(script.started(&hash2))
            .await
            .expect("script did not run for hash2");
        timeout_1s(started_hash1.siginted())
            .await
            .expect("hash1 test did not get siginted");
        expect_result_1s(
            &mut results,
            CommitTestResult {
                hash: hash1,
                result: Ok(TestOutcome::Canceled),
            },
        )
        .await
        .unwrap();
    }

    // TODO: test with variations of nthreads size and queue depth.
    // TODO: test starting up on an empty repo?
    // TODO: test only one worker task (I think this is actually broken)
    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
