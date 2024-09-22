use core::fmt;
use core::fmt::Display;
use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::ffi::OsString;
use std::fmt::Debug;
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context};
use futures::future::{self, try_join_all, Either};
use log::debug;
use log::error;
use log::info;
use log::warn;
use nix::sys::signal::kill;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use serde::Deserialize;
use serde::Serialize;
use tokio::process::Command;
use tokio::select;
use tokio::sync::broadcast;
use tokio::sync::watch;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::git::TempWorktree;
use crate::git::{CommitHash, Hash, Worktree};
use crate::process::OutputExt;
use crate::resource::Pools;
use crate::result::Database;
use crate::result::TestCaseOutput;

pub trait ResultExt {
    // Log an error if it occurs, prefixed with s, otherwise return nothing.
    fn or_log_error(&self, s: &str);
}

impl<T, E> ResultExt for Result<T, E>
where
    E: Debug,
{
    fn or_log_error(&self, s: &str) {
        if let Err(e) = self {
            error!("{} - {:?}", s, e);
        }
    }
}

#[derive(Deserialize, Serialize, Debug, Clone, Copy, Hash, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    NoCaching,
    ByCommit,
    ByTree,
}

// Some unspecified hash, don't care too much about stability across builds.
pub type ConfigHash = u64;

// A test task that will need to be repeated for each commit.
// We have tests that use these as hash keys for historical reasons. I think
// this is fine but I'm not sure if it's really something to reasonably take
// advantage of in prod code so we only derive the necessary traits for test.
#[cfg_attr(test, derive(Hash, PartialEq, Eq))]
pub struct Test {
    pub name: String,
    // Hash of the configuration that created this Test.
    pub config_hash: ConfigHash,
    pub program: OsString,
    pub args: Vec<OsString>,
    // Indexes of pools in the Manager's token_pools from which this test needs
    // a resource-token before it can begin.
    pub needs_resource_idxs: Vec<usize>,
    pub shutdown_grace_period: Duration,
    pub cache_policy: CachePolicy,
}

impl Test {
    fn command(&self) -> Command {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        // Ensure we don't pass random nonsense to the test command and create
        // confusing behaviour. This is kinda annoying because IIUC this gives
        // you a fd that is immediately closed, which is likely to be different
        // from the environment where you're testing your scripts (i.e. a shell
        // prompt). You'd think just using Stdio::piped() would give a stdin
        // that is open but has nothing on it, but that isn't th behavoiur I've
        // observed, I'm not too sure why but don't wanna keep debugging this
        // forever.
        cmd.stdin(Stdio::null());
        cmd
    }
}

impl Display for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "<test: {:?}>", self.name)
    }
}

impl Debug for Test {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self)
    }
}

pub struct ManagerBuilder<W> {
    // This needs to be an Arc because we hold onto a reference to it for a
    // while, and create temporary worktrees from it in the background.
    repo: Arc<W>,
    tests: Vec<Test>,
    token_pool_sizes: Vec<usize>,
    result_db: Database,

    num_worktrees: usize,
    worktree_prefix: String,
    worktree_dir: PathBuf,
}

impl<W> ManagerBuilder<W> {
    pub fn num_worktrees(mut self, n: usize) -> Self {
        self.num_worktrees = n;
        self
    }

    // Worktree temp-directories will have their name (not path!) prefixed with this.
    pub fn worktree_prefix(mut self, prefix: &str) -> Self {
        prefix.clone_into(&mut self.worktree_prefix);
        self
    }

    // Directory to create worktrees in
    pub fn worktree_dir<P: Into<PathBuf>>(mut self, dir: P) -> Self {
        self.worktree_dir = dir.into();
        self
    }

    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    pub async fn build(self) -> anyhow::Result<Manager<W>>
    where
        // We need to specify 'static here. Just because we have an Arc over the
        // repo that doesn't mean it automatically satisfies 'static:
        // https://users.rust-lang.org/t/why-is-t-static-constrained-when-using-arc-t-and-thread-spawn/26262/2
        // It would be much more convenient to just specify some or all these
        // trait bounds as subtraits of Workrtree. But I dunno, that feels Wrong.
        W: Worktree + Sync + Send + 'static,
    {
        info!(
            "Setting up {} worktrees in {:?}...",
            self.num_worktrees, self.worktree_dir
        );
        let worktrees = try_join_all((0..self.num_worktrees).map(|_| async {
            // Not doing this async because I assume it's fast, there is no white-glove support,
            // and the drop will have to be synchronous anyway.
            let t = tempfile::Builder::new()
                .prefix(&self.worktree_prefix)
                .tempdir_in(&self.worktree_dir)
                .context("creating temp dir for worktree")?;
            debug!("Creating worktree: {:?}", t.path());
            TempWorktree::new::<W>(self.repo.borrow(), t).await
        }))
        .await
        .context("setting up temporary worktrees")?;
        info!("Worktree setup done.");
        // TODO: If this capacity gets exhausted, data gets lost and we get an error which this code
        // probably doesn't handle very gracefully. We should instead just block the sender.
        let (result_tx, _) = broadcast::channel(4096);
        Ok(Manager {
            origin_path: Arc::new(self.repo.path().to_owned()),
            repo: self.repo,
            result_tx,
            job_cts: HashMap::new(),
            job_counter: JobCounter::new(),
            tests: self.tests.into_iter().map(Arc::new).collect(),
            resource_pools: Arc::new(Pools::new(self.token_pool_sizes, worktrees)),
            result_db: self.result_db,
        })
    }
}

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager<W: Worktree> {
    repo: Arc<W>,
    job_cts: HashMap<CommitHash, CancellationToken>,
    job_counter: JobCounter,
    result_tx: broadcast::Sender<Arc<Notification>>,
    tests: Vec<Arc<Test>>,
    // Pools contains sets of intangible arbitrary "resources" that can be used to throttle test
    // jobs, and also tracks access to reused worktrees. The indices of the token-type resources
    // will be referenced by Test::needs_resource_idx values.
    resource_pools: Arc<Pools<TempWorktree>>,
    result_db: Database,
    // Path of original repo.
    origin_path: Arc<PathBuf>,
}

impl<W: Worktree> Manager<W> {
    pub fn builder<I: IntoIterator<Item = Test>, J: IntoIterator<Item = usize>>(
        // This needs to be an Arc because we hold onto a reference to it for a
        // while, and create temporary worktrees from it in the background.
        repo: Arc<W>,
        // This is mandatory instead of defaulting to the user's main database,
        // because we want it to be hard to accidentally refer to global
        // resources like that.
        result_db: Database,
        tests: I,
        token_pool_sizes: J,
    ) -> ManagerBuilder<W> {
        ManagerBuilder {
            repo,
            tests: tests.into_iter().collect(),
            token_pool_sizes: token_pool_sizes.into_iter().collect(),
            result_db,

            num_worktrees: 1,
            worktree_prefix: "worktree-".to_owned(),
            worktree_dir: env::temp_dir(),
        }
    }

    async fn cache_lookup(&self, test_case: &TestCase) -> Option<TestResult> {
        match test_case.cache_hash {
            Some(ref hash) => {
                match self
                    .result_db
                    .cached_result(hash, &test_case.test.name, test_case.test.config_hash)
                    .context("reading cached test result")
                {
                    Err(err) => {
                        error!("Failed to read cached test result, will overwrite: {err:?}");
                        None
                    }
                    Ok(maybe_result) => maybe_result,
                }
            }
            None => None,
        }
    }

    fn notify(&self, test_case: TestCase, status: TestStatus) {
        self.result_tx
            .send(Arc::new(Notification { test_case, status }))
            .or_log_error("Dropping a notification. Probably nothing was listening");
    }

    fn spawn_job(&self, mut job: TestJob) {
        self.notify(job.test_case.clone(), TestStatus::Enqueued);

        let tx = self.result_tx.clone();
        let pools = self.resource_pools.clone();
        tokio::spawn(async move {
            // This "biased" is here because otherwise when we cancel a bunch of jobs all at once,
            // and some of those jobs are blocking on resources held by others,
            // we want the former jobs to observe their own cancellation before
            // they see the resources get freed up by the latter. I don't think
            // this totally eliminates that case, which probably means tests
            // will be flaky. Not sure what to do about that.
            select!(biased;
                    _ = job.ct.cancelled() => (),
                    resources = pools.get(job.test_case.test.needs_resource_idxs.clone()) =>  {
                tx.send(Arc::new(Notification {
                    test_case: job.test_case.clone(),
                    status: TestStatus::Started,
                }))
                .or_log_error("Dropping a notification");
                let worktree = resources.obj();
                let result = job.run(worktree).await;
                let status = match result {
                    Err(ref err) => TestStatus::Error(err.to_string()),
                    Ok(None) => TestStatus::Canceled,
                    Ok(Some(exit_code)) => {
                        let test_result = TestResult{exit_code};
                        job.output
                            .set_result(&test_result)
                            .or_log_error("couldn't save job status");
                        TestStatus::Completed(test_result)
                    }
                };
                // Note: must not drop test until the send is complete, or we would break
                // settled().
                let _ = tx.send(Arc::new(Notification {
                    test_case: job.test_case,
                    status,
                }))
                .map_err(|e|
                    error!("Dropping a result ({result:?}. Seems nobody is listening to Manager::results(): {}", e)
                );
            });
        });
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    // It doesn't make sense to call this function if you don't have a receiver
    // from already having called [[results]].
    pub async fn set_revisions<I>(&mut self, revs: I) -> anyhow::Result<()>
    where
        I: IntoIterator<Item = CommitHash>,
    {
        let mut to_start = HashSet::<CommitHash>::from_iter(revs);
        let mut cancel_revs = Vec::new();
        for rev in self.job_cts.keys() {
            // We're already testing rev, so we don't need to kick it off below.
            if !to_start.remove(rev) {
                // This rev is being tested but wasn't in rev_set.
                cancel_revs.push(rev.clone())
            }
        }
        info!("Enqueueing {:?}, cancelling {:?}", to_start, cancel_revs);
        for rev in cancel_revs {
            self.job_cts[&rev].cancel();
            self.job_cts.remove(&rev);
        }

        for rev in to_start {
            for test in self.tests.iter() {
                let test_case = TestCase::new(rev.to_owned(), test.clone(), self.repo.as_ref())
                    .await
                    .context("failed to create TestCase")?;
                if let Some(test_result) = self.cache_lookup(&test_case).await {
                    self.notify(test_case, TestStatus::Completed(test_result));
                    continue;
                }

                let ct = CancellationToken::new();
                self.job_cts.insert(rev.to_owned(), ct.clone());
                self.spawn_job(TestJob {
                    ct,
                    _token: self.job_counter.get(),
                    output: self.result_db.create_output(
                        test_case.storage_hash(),
                        &test_case.test.name,
                        test_case.test.config_hash,
                    )?,
                    test_case,
                    origin_path: self.origin_path.clone(),
                });
            }
        }
        Ok(())
    }

    // Streams results back. Note you need to call this _before_ you generate the results you want
    // to receive.
    //
    // I think the "proper" solution for this is to return a Stream. But I don't understand it.
    pub fn results(&self) -> broadcast::Receiver<Arc<Notification>> {
        self.result_tx.subscribe()
    }

    // Completes once there are no pending jobs or results.
    pub async fn settled(&self) {
        self.job_counter.zero().await;
    }
}

// This is a horrible attempt to implement Manager::settled. There is no Condvar in tokio or
// futures-rs, so we have this weird condvar-like construction using a Tokio watch channel.
struct JobCounter {
    w: watch::Sender<usize>,
}

impl JobCounter {
    pub fn new() -> Self {
        Self {
            w: watch::Sender::new(0),
        }
    }

    // Increment the counter. It is decremented when the token is dropped.
    pub fn get(&self) -> JobToken {
        // Hack? We only report that we "modified" the value if it changed its
        // zeroness, since that's the only thing that waiters care about.
        self.w.send_if_modified(|count| {
            let was_zero = *count == 0;
            *count += 1;
            was_zero
        });
        JobToken { w: self.w.clone() }
    }

    // Block until the counter is zero. If it's already zero, return immediately. This might miss
    // transient zeroness but is guaranteed to return eventually if the counter stays zero for some
    // finite amount of time.
    pub async fn zero(&self) {
        let mut rx = {
            if *self.w.borrow() == 0 {
                return;
            }
            // Note there's a race here, we've already dropped the Ref from self.w.borrow() so the
            // counter could change. This doesn't matter because wait_for checks if the value is
            // already zero before blocking, so missed updates are harmless.
            self.w.subscribe()
        };
        rx.wait_for(|count| *count == 0)
            .await
            .expect("sender dropped in job counter");
    }
}

struct JobToken {
    w: watch::Sender<usize>,
}

impl Drop for JobToken {
    fn drop(&mut self) {
        self.w.send_if_modified(|count| {
            *count -= 1;
            *count == 0
        });
    }
}

// This is not really a proper type, it doesn't really mean anything except as an implementation
// detail of its user. I tried to get rid of it but then you run into issues with getting references
// to individual fields while a mutable reference exists to the overall struct. I think this is
// basically one an instance of "view structs" described in
// https://smallcultfollowing.com/babysteps/blog/2024/06/02/the-borrow-checker-within/
struct TestJob {
    ct: CancellationToken,
    test_case: TestCase,
    _token: JobToken,
    output: TestCaseOutput,
    // Path of original repo we are testing (not our worktree).
    origin_path: Arc<PathBuf>,
}

impl TestJob {
    // Returns Ok(None) when canceled.
    async fn run<W>(&mut self, worktree: &W) -> anyhow::Result<Option<ExitCode>>
    where
        W: Worktree,
    {
        info!(
            "Starting {} for rev {}...",
            self.test_case.test.name, self.test_case.commit_hash
        );

        worktree.checkout(&self.test_case.commit_hash).await?;

        let mut cmd = self.test_case.test.command();
        let child = cmd
            .current_dir(worktree.path())
            .stdout(self.output.stdout().context("no stdout handle available")?)
            .stderr(self.output.stderr().context("no stdout handle available")?)
            .env("LCI_COMMIT", self.test_case.commit_hash.to_string())
            .env("LCI_ORIGIN", self.origin_path.as_os_str())
            // Killing on drop is not what we want. We really want this job to
            // get awaited so that the worktree can be safely reused and we can
            // be sure the test script has cleaned up after itself. But, in case
            // local-ci shuts down unexpectedly we'll try to at least limit the
            // damage.
            .kill_on_drop(true)
            .spawn()
            .context("spawning test command")?;
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
            Either::Left((wait_result, _)) =>
            // Test completed, figure out the result. I think maybe a true Rustacean would
            // write this block as a single chain of methods? But it seems ridiculous to me.
            {
                let exit_code = wait_result
                    .map_err(anyhow::Error::from)?
                    .code_not_killed()?;
                Ok(Some(exit_code))
            }
            Either::Right((_, child_fut)) => {
                // Canceled. Shut down the process.
                kill(pid, Signal::SIGINT).context("couldn't interrupt child job")?;
                // We don't care about its result but we
                // need to wait for it to shut down so that we can safely give back the
                // worktree.
                let timeout = sleep(self.test_case.test.shutdown_grace_period);
                select!(
                    _ = child_fut => (),
                    _ = timeout => {
                        // Canceled. Shut down the process.
                        warn!("timeout for {:?}, SIGKILLing", self.test_case.test.name);
                        kill(pid, Signal::SIGKILL).context("couldn't interrupt child job")?;
                    }
                );

                Ok(None)
            }
        }
    }
}

#[derive(Debug, Clone)]
#[cfg_attr(test, derive(Hash, PartialEq, Eq))] // See comment on Test.
pub struct TestCase {
    // Commit that will be checked out to run the test.
    pub commit_hash: CommitHash,
    // Hash that will be used to identify the test result. Might be a tree hash,
    // otherwise it matches the commit hash.
    pub cache_hash: Option<Hash>,
    pub test: Arc<Test>,
}

impl TestCase {
    pub async fn new<W: Worktree>(
        commit_hash: CommitHash,
        test: Arc<Test>,
        repo: &W,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            cache_hash: match test.cache_policy {
                CachePolicy::NoCaching => None::<Hash>,
                CachePolicy::ByCommit => Some(commit_hash.clone().into()),
                CachePolicy::ByTree => Some(
                    repo.commit_tree(commit_hash.clone())
                        .await
                        .context("looking up tree from commit")?
                        .into(),
                ),
            },
            test,
            commit_hash,
        })
    }

    // Returns the hash that should be used to store the result in the result
    // database. Note that results get stored in the database even when caching
    // is disabled, so that the user can see the output..
    pub fn storage_hash(&self) -> &Hash {
        self.cache_hash
            .as_ref()
            .unwrap_or(self.commit_hash.as_ref())
    }
}

pub type ExitCode = i32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TestStatus {
    Enqueued,
    Started,
    Canceled,
    // anyhow::Error doesn't implement Clone. We don't really need the overall
    // error handling fanciness since this is just part of the normal flow of
    // the program, so we just define this as a normal case among this enum.
    Error(String), // This includes the test getting terminated by a signal.
    Completed(TestResult),
}

impl Display for TestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Enqueued => write!(f, "Enqueued"),
            Self::Started => write!(f, "Started"),
            Self::Canceled => write!(f, "Cancelled"),
            Self::Error(msg) => write!(f, "Failed testing - {:?}", msg),
            Self::Completed(result) => write!(f, "Completed - {}", result),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct TestResult {
    // Note this is called "exit_code" instead of "return_code" because it really
    // only gets set when the child process exits.
    pub exit_code: ExitCode,
}

impl Display for TestResult {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "exit code {}", self.exit_code)
    }
}

#[derive(Debug)]
pub struct Notification {
    pub test_case: TestCase,
    pub status: TestStatus,
}

#[cfg(test)]
mod tests {
    use std::{
        array,
        collections::VecDeque,
        fs::{self, remove_file, File},
        io::{self, BufRead as _},
        path::PathBuf,
        thread::panicking,
        time::Duration,
    };

    use anyhow::bail;
    use future::select_all;
    use log::error;
    use tempfile::TempDir;
    use test_case::test_case;
    use tokio::{
        select,
        time::{sleep, sleep_until, Instant},
    };

    use crate::{
        git::{
            test_utils::{TempRepo, WorktreeExt},
            CommitHash,
        },
        test_utils::{path_exists, some_time, timeout_5s},
    };

    use super::*;

    // A script that can be used as the test command for a Manager, with utilities for testing the
    // manager. The script won't terminate until told to.
    struct TestScript {
        dir: TempDir,
        script: OsString, // Raw content.
    }

    impl TestScript {
        // Each time the script gets started it echoes a line to this file.
        const PID_FILENAME_PREFIX: &'static str = "pid.";
        const SIGINTED_FILENAME_PREFIX: &'static str = "siginted.";
        const LOCK_FILENAME: &'static str = "lockfile";
        const BUG_DETECTED_PATH: &'static str = "bug_detected";

        // If this appears in the commit message , the test script will block until SIGINTed,
        // otherwise it terminates immediately.
        pub const BLOCK_COMMIT_MSG_TAG: &'static str = "block_this_test";

        // Generate a tag which, when put in the commit message of a commit, will result in the test
        // returning the given exit code.
        pub fn exit_code_tag(code: u32) -> OsString {
            format!("exit_code({})", code).into()
        }

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        pub fn new() -> Self {
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
                echo $$ >> {pid_path_prefix:?}$(git rev-parse HEAD)

                if [ -e ./{lock_filename:?} ]; then
                    echo 'Overlapping test script runs used the same worktree' >> {bug_detected_path:?}
                fi
                trap \"rm {lock_filename:?}\" EXIT
                touch ./{lock_filename:?}
                commit_msg=\"$(git log -n1 --format=%B)\"
                if [[ \"$commit_msg\" =~ {block_tag} ]]; then
                    # sleep is not a builtin so we won't handle SIGINT while
                    # that's running. Hack suggested by ChatGPT: just spawn it
                    # then use wait, which is a builtin.
                    sleep infinity &
                    wait $!
                fi
                # Extract the exit code and pass it to exit if there is one, otherwise pass 0.
                exit_code=$(echo \"$commit_msg\" | perl -n -e'/exit_code\\((\\d+)\\)/ && print $1')
                exit ${{exit_code:-0}}
                ",
                pid_path_prefix = dir.path().join(Self::PID_FILENAME_PREFIX),
                siginted_path_prefix = dir.path().join(Self::SIGINTED_FILENAME_PREFIX),
                lock_filename = Self::LOCK_FILENAME,
                bug_detected_path = dir.path().join(Self::BUG_DETECTED_PATH),
                block_tag = Self::BLOCK_COMMIT_MSG_TAG,
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
            filename.push(hash);
            self.dir.path().join(filename)
        }

        // If this path exists, two instances of the script used the same worktree at once.
        fn bug_detected_path(&self) -> PathBuf {
            self.dir.path().join(Self::BUG_DETECTED_PATH)
        }

        // Has the script been started so far for this test?
        pub fn was_started(&self, hash: &CommitHash) -> bool {
            self.signalling_path(Self::PID_FILENAME_PREFIX, hash)
                .exists()
        }

        // Blocks until the script is started for the given commit hash.
        pub async fn started(&self, hash: &CommitHash) -> StartedTestScript {
            let pid_path = self.signalling_path(Self::PID_FILENAME_PREFIX, hash);
            path_exists(&pid_path).await;
            let content = fs::read_to_string(pid_path).expect("couldn't read PID file");
            // The script appends a PID to the file each time, this is handy for num_runs.
            // So we just take the last one.
            let line = content
                .trim()
                .split("\n")
                .last()
                .expect("PID file existed but contained no PIDs");
            StartedTestScript {
                script: self,
                hash: hash.to_owned(),
                pid: Pid::from_raw(line.parse().unwrap_or_else(|_| {
                    panic!("couldn't parse PID file as integer (content: {content:?}")
                })),
            }
        }

        // Number of times the script has been successfully spawned for this
        // commit, since StartedTestScript::reset_started was called for it.
        pub fn num_runs(&self, hash: &CommitHash) -> usize {
            let path = self.signalling_path(Self::PID_FILENAME_PREFIX, hash);
            if !path.exists() {
                return 0;
            }
            // Awkward: it's probably still possible to return 0 here, we can
            // probably observe the file whe it exists but doesn't have any content yet.
            io::BufReader::new(File::open(path).expect("couldn't open TestScript signalling file"))
                .lines()
                .count()
        }

        pub fn test_name(&self) -> &str {
            "my_test"
        }

        pub fn as_test(&self, cache_policy: CachePolicy) -> Test {
            Test {
                name: self.test_name().to_owned(),
                program: self.program(),
                args: self.args(),
                needs_resource_idxs: vec![],
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy,
                config_hash: 0,
            }
        }
    }

    // Hack to check for stuff that is orthogonal to any particular test, so we
    // don't wanna have to it in every individual test.
    impl Drop for TestScript {
        fn drop(&mut self) {
            let path = self.bug_detected_path();
            if path.exists() {
                let content =
                    std::fs::read_to_string(path).expect("couldn't read bug-detected path");
                let msg = format!("The test script detected one or more bugs: {}", content);
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
        pid: Pid,
    }

    impl<'a> StartedTestScript<'a> {
        // Blocks until the script has received a SIGINT.
        pub async fn siginted(&self) {
            path_exists(
                self.script
                    .signalling_path(TestScript::SIGINTED_FILENAME_PREFIX, &self.hash),
            )
            .await;
        }

        // SIGTERM the instance of the script. Use this if you want the script
        // to "fail with an error". This preferable to SIGKILL because that will
        // prevent the underlying script from performing its cleanup.
        pub fn sigterm(&self) {
            kill(self.pid, Signal::SIGTERM).expect("couldn't SIGKILL test script");
        }

        // Forget "started" state so that TestScript::started can usefully be called again.
        pub fn reset_started(self) {
            remove_file(
                self.script
                    .signalling_path(TestScript::PID_FILENAME_PREFIX, &self.hash),
            )
            .expect("couldn't delete test script PID file");
        }
    }

    fn dump_want_statuses(want: &HashMap<TestCase, VecDeque<TestStatus>>) -> String {
        let mut ret = String::from("");
        for (test_case, statuses) in want {
            ret.push_str(&format!("{:?}\n", test_case));
            for status in statuses {
                ret.push_str(&format!("\t{:?}\n", status));
            }
        }
        ret
    }
    // Expect the series of notifications provided for each test case.
    // case. Also assert that the necessary precursor notifications arrive.
    // Panics if any of the input series are empty.
    async fn expect_notifs_10s(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        mut want: HashMap<TestCase, VecDeque<TestStatus>>,
    ) -> anyhow::Result<()> {
        let timeout = Instant::now() + Duration::from_secs(10);
        while !want.is_empty() {
            let notif = select!(
                _ = sleep_until(timeout) => {
                    bail!("timeout after 5s, remaining results:\n{}",
                        dump_want_statuses(&want));
                },
                output = results.recv() => {
                    output.context(format!(
                        "test result stream terminated, remaining results:\n{}",
                        dump_want_statuses(&want)))?
                }
            );
            let want_statuses = want.get_mut(&notif.test_case).context(format!(
                "got notification for unexpected test case: {notif:?}",
            ))?;
            let want_status = want_statuses.pop_front().expect("empty status series");
            if want_statuses.is_empty() {
                want.remove(&notif.test_case);
            }
            if notif.status != want_status {
                bail!(
                    "unexpected test notification for {:?}, got {:?} want {:?}",
                    notif.test_case,
                    notif.status,
                    want_status
                );
            }
        }
        Ok(())
    }

    async fn expect_no_more_results(
        results: &mut broadcast::Receiver<Arc<Notification>>,
        m: &Manager<TempRepo>,
    ) -> anyhow::Result<()> {
        select!(
            _ = sleep(Duration::from_secs(1)) => bail!("didn't settle after 1s"),
            result = results.recv() => bail!("unexpected test result received: {:?}", result),
            _ = m.settled() => Ok(())
        )
    }

    struct TestScriptFixture<const NUM_TESTS: usize> {
        _db_dir: TempDir,
        repo: Arc<TempRepo>,
        scripts: [TestScript; NUM_TESTS],
        manager: Manager<TempRepo>,
    }

    impl<const NUM_TESTS: usize> TestScriptFixture<NUM_TESTS> {
        // Agh, it's really hard to make an ergonomic API for configuring these
        // fixtures. Do we need a builder struct? Yuck. For now we just have one
        // really ugly "verbose" constructor and then others that call this one.
        // It's like C!
        async fn new_verbose(
            num_worktrees: usize,
            cache_policies: [CachePolicy; NUM_TESTS],
        ) -> Self {
            let repo = Arc::new(TempRepo::new().await.unwrap());
            // TODO: We need to have a commit in the repo otherwise manager
            // setup will fail. This is a bug.
            repo.commit("hello,", some_time())
                .await
                .expect("couldn't create base commit");
            let scripts = array::from_fn(|_| TestScript::new());
            let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
            let manager = Manager::builder(
                repo.clone(),
                Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
                scripts
                    .iter()
                    .zip(cache_policies)
                    .map(|(s, c)| s.as_test(c)),
                [],
            )
            .num_worktrees(num_worktrees)
            .build()
            .await
            .expect("couldn't set up manager");
            Self {
                manager,
                scripts,
                repo,
                _db_dir: db_dir,
            }
        }

        async fn new_with_worktrees(num_worktrees: usize) -> Self {
            Self::new_verbose(num_worktrees, [CachePolicy::ByCommit; NUM_TESTS]).await
        }

        async fn new_with_cache_policies(cache_policies: [CachePolicy; NUM_TESTS]) -> Self {
            Self::new_verbose(2, cache_policies).await
        }

        async fn new() -> Self {
            Self::new_with_worktrees(2).await
        }

        // Convenience helper to construct a TestCase referring to this fixture's configuration.
        async fn test_case(&self, hash: impl Borrow<CommitHash>, test_idx: usize) -> TestCase {
            TestCase::new(
                hash.borrow().to_owned(),
                self.manager.tests[test_idx].clone(),
                self.repo.as_ref(),
            )
            .await
            .unwrap()
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let mut f = TestScriptFixture::<1>::new().await;
        let mut results = f.manager.results();
        let hash = f
            .repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions(vec![hash.clone()]).await.unwrap();
        // We should get a singular result because we only fed in one revision.
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hash, 0).await,
                vec![
                    TestStatus::Enqueued,
                    TestStatus::Started,
                    TestStatus::Completed(TestResult { exit_code: 0 }),
                ]
                .into(),
            )]),
        )
        .await
        .expect("bad test result");
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let mut f = TestScriptFixture::<1>::new().await;
        // First commit's test will block forever.
        let hash1 = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        let mut results = f.manager.results();
        f.manager.set_revisions(vec![hash1.clone()]).await.unwrap();
        let started_hash1 = timeout_5s(f.scripts[0].started(&hash1))
            .await
            .expect("script did not run for hash1");
        // Second commit's test will terminate quickly.
        let hash2 = f
            .repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions(vec![hash2.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&hash2))
            .await
            .expect("f.scripts[0] did not run for hash2");
        timeout_5s(started_hash1.siginted())
            .await
            .expect("hash1 test did not get siginted");
        expect_notifs_10s(
            &mut results,
            // awu weh, weh mah
            HashMap::from([
                (
                    f.test_case(hash1, 0).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Canceled,
                    ]
                    .into(),
                ),
                // This isn't what we're testing here but we need to assert that it comes in so we can
                // check below that nothing else comes in.
                (
                    f.test_case(hash2, 0).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Completed(TestResult { exit_code: 0 }),
                    ]
                    .into(),
                ),
            ]),
        )
        .await
        .unwrap();
        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap()
    }

    // This is not actually testing functionality, this is a meta-test, yikes this is
    // over-engineered.
    #[test_log::test(tokio::test)]
    async fn should_not_settle() {
        let mut f = TestScriptFixture::<1>::new().await;
        // First commit's test will block forever.
        let hash = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");
        f.manager.set_revisions([hash.clone()]).await.unwrap();
        timeout_5s(f.scripts[0].started(&hash))
            .await
            .expect("script did not start");
        select!(
            _ = sleep(Duration::from_secs(1)) => (),
            _ = f.manager.settled() => panic!("manager settled unexpectedly"),
        )
    }

    #[test_log::test(tokio::test)]
    async fn should_cache_results() {
        let mut f = TestScriptFixture::new_with_cache_policies([
            CachePolicy::NoCaching,
            CachePolicy::ByCommit,
            CachePolicy::ByTree,
        ])
        .await;
        f.repo
            .commit("yarp", some_time())
            .await
            .expect("couldn't create test commit");

        // Set up two commits to test. Note this is a bit of an odd test case.
        // The real world case we are thinking of here is probably swiching
        // between two branches with a common base rather than two ranges with
        // an ancestry relation. But to do that we'd need more helpers in our
        // Git library. So we just take advantage of our knowledge that this
        // weirdness of the test case is irrelevant given how the implementation
        // works (it doesn't know about the structure of the history).
        let orig_hash = f
            .repo
            .commit("yarp", some_time())
            .await
            .expect("couldn't create test commit");
        // This one has a different commit hash but the same tree.
        let same_tree = f
            .repo
            .commit("darp", some_time())
            .await
            .expect("couldn't create test commit");

        // Test the first commit and wait for it to be complete.
        f.manager
            .set_revisions(vec![orig_hash.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Sanity check that the scripts actually got run.
        assert_eq!(f.scripts[0].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[1].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_hash), 1);

        f.manager
            .set_revisions(vec![same_tree.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        // Without caching the script should get run every time.
        assert_eq!(f.scripts[0].num_runs(&same_tree), 1);
        // Commit has has changed so new commit should get retested for ByCommit.
        assert_eq!(f.scripts[1].num_runs(&same_tree), 1);
        // But not for ByTree
        assert_eq!(f.scripts[2].num_runs(&same_tree), 0);

        f.manager
            .set_revisions(vec![orig_hash.clone()])
            .await
            .unwrap();
        f.manager.settled().await;
        assert_eq!(f.scripts[0].num_runs(&orig_hash), 2);
        assert_eq!(f.scripts[1].num_runs(&orig_hash), 1);
        assert_eq!(f.scripts[2].num_runs(&orig_hash), 1);
    }

    #[test_case(1, 1 ; "single worktree, one test")]
    #[test_case(4, 1 ; "multiple worktrees, one test")]
    #[test_case(4, 4 ; "multiple worktrees, multiple tests")]
    #[test_log::test(tokio::test)]
    async fn should_handle_many(num_worktrees: usize, num_tests: usize) {
        let mut f = TestScriptFixture::<1>::new_with_worktrees(num_worktrees).await;
        let mut hashes = Vec::new();
        let mut want_results = HashMap::new();
        let mut i = 0;
        for _ in 0..50 {
            for _ in 0..num_tests {
                let hash = f
                    .repo
                    // We'll give each test a unique exit code so we can check they really got
                    // tested individually.
                    .commit(TestScript::exit_code_tag(i as u32), some_time())
                    .await
                    .expect("couldn't create test commit");
                want_results.insert(
                    f.test_case(&hash, 0).await,
                    vec![
                        TestStatus::Enqueued,
                        TestStatus::Started,
                        TestStatus::Completed(TestResult { exit_code: i }),
                    ]
                    .into(),
                );
                hashes.push(hash);
                i += 1;
            }
        }
        let mut results = f.manager.results();
        f.manager.set_revisions(hashes.clone()).await.unwrap();
        expect_notifs_10s(&mut results, want_results)
            .await
            .expect("bad results");
    }

    #[test_log::test(tokio::test)]
    async fn should_respect_resource_limits() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let mut hashes = Vec::new();
        for _ in 0..10 {
            hashes.push(
                repo.commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
                    .await
                    .expect("couldn't create test commit"),
            );
        }
        let script = TestScript::new();
        // We only have 2 tokens
        let resource_token_counts = [2];
        // And a test that requires one of those tokens.
        let tests = [Test {
            name: "my_test".to_owned(),
            program: script.program(),
            args: script.args(),
            needs_resource_idxs: vec![1],
            shutdown_grace_period: Duration::from_secs(5),
            cache_policy: CachePolicy::ByCommit,
            config_hash: 0,
        }];
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let mut m = Manager::builder(
            repo.clone(),
            Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
            tests,
            resource_token_counts,
        )
        .num_worktrees(4)
        .build()
        .await
        .expect("couldn't set up manager");
        m.set_revisions(hashes.clone()).await.unwrap();

        let mut start_futs = hashes.iter().map(|h| Box::pin(script.started(h))).collect();
        for _ in 0..2 {
            let (_started, _index, remaining) = timeout_5s(select_all(start_futs))
                .await
                .expect("didn't start first two jobs");
            start_futs = remaining;
        }

        // Ugh, dunno how to do this except just wait for 1s...
        select!(
            _ = sleep(Duration::from_secs(1)) => (), // OK, nothing else ran.
            _ = select_all(start_futs) => panic!("extra jobs started, resource limits not respected"),
        )
    }

    #[test_log::test(tokio::test)]
    async fn test_job_env() {
        let temp_dir = TempDir::new().unwrap();
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let hash = repo
            .commit("hello,", some_time())
            .await
            .expect("couldn't create test commit");
        let db_dir = TempDir::new().expect("couldn't make temp dir for result DB");
        let mut m = Manager::builder(
            repo.clone(),
            Database::create_or_open(db_dir.path()).expect("couldn't setup result DB"),
            [Test {
                name: "my_test".to_owned(),
                program: OsString::from("bash"),
                args: vec![
                    "-c".into(),
                    OsString::from(format!(
                        "echo $LCI_ORIGIN >> {0:?}/lci_origin && echo $LCI_COMMIT >> {0:?}/lci_commit",
                        temp_dir.path()
                    )),
                ],
                needs_resource_idxs: vec![],
                shutdown_grace_period: Duration::from_secs(5),
                cache_policy: CachePolicy::ByCommit,
                config_hash: 0
            }],
            [],
        )
        .build()
        .await
        .expect("couldn't set up manager");

        m.set_revisions([hash.clone()])
            .await
            .expect("set_revisions failed");
        m.settled().await;

        assert_eq!(
            fs::read_to_string(temp_dir.path().join("lci_origin"))
                .expect("couldn't read lci_origin from test script")
                .trim(),
            repo.path().to_string_lossy()
        );
        assert_eq!(
            CommitHash::new(
                fs::read_to_string(temp_dir.path().join("lci_commit"))
                    .expect("couldn't read lci_origin from test script")
                    .trim()
            ),
            hash,
        );
    }

    #[test_log::test(tokio::test)]
    async fn should_not_start_canceled() {
        let mut f = TestScriptFixture::<1>::new_with_worktrees(2).await;
        let mut results = f.manager.results();
        let mut hashes = Vec::new();
        for _ in 0..5 {
            hashes.push(
                f.repo
                    .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
                    .await
                    .expect("couldn't create test commit"),
            );
        }

        // We're gonna start one test and have another become blocked waiting
        // for a worktree. In order to make it deterministic which one gets
        // blocked, we'll do this in two phases.
        f.manager
            .set_revisions(vec![hashes[0].clone()])
            .await
            .unwrap();
        // wait for first test to get started.
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hashes[0], 0).await,
                vec![TestStatus::Enqueued, TestStatus::Started].into(),
            )]),
        )
        .await
        .expect("bad test result");

        // Now we enqueue the test that should block.
        f.manager
            .set_revisions(vec![hashes[0].clone(), hashes[1].clone()])
            .await
            .unwrap();
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hashes[1], 0).await,
                vec![TestStatus::Enqueued].into(),
            )]),
        )
        .await
        .expect("bad test result");

        // Now we cancel both of those tests.
        f.manager.set_revisions(Vec::new()).await.unwrap();
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hashes[0], 0).await,
                vec![TestStatus::Canceled].into(),
            )]),
        )
        .await
        .expect("bad test result");

        expect_no_more_results(&mut results, &f.manager)
            .await
            .unwrap();

        assert!(!f.scripts[0].was_started(&hashes[1]));
    }

    #[test_log::test(tokio::test)]
    async fn should_not_cache() {
        let mut f = TestScriptFixture::<2>::new_with_worktrees(2).await;
        let mut results = f.manager.results();
        let hash = f
            .repo
            .commit(TestScript::BLOCK_COMMIT_MSG_TAG, some_time())
            .await
            .expect("couldn't create test commit");

        // Wait until both tests are started.
        f.manager
            .set_revisions(vec![hash.clone()].clone())
            .await
            .unwrap();
        expect_notifs_10s(
            &mut results,
            HashMap::from([
                (
                    f.test_case(&hash, 0).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
                (
                    f.test_case(&hash, 1).await,
                    vec![TestStatus::Enqueued, TestStatus::Started].into(),
                ),
            ]),
        )
        .await
        .expect("bad test result");

        // Cause one to fail with an error. We take advantage of the fact that
        // this whole tool considers it an "error" when a test exits with a
        // signal instead of exiting with a nonzero code.
        // f.scripts[0];
        let started_script = f.scripts[0].started(&hash).await;
        started_script.sigterm();
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hash, 0).await,
                vec![TestStatus::Error(String::from("terminated by signal 15"))].into(),
            )]),
        )
        .await
        .expect("didn't get error after killing script");
        started_script.reset_started();

        // ... and the other to be canceled.
        f.manager.set_revisions(vec![]).await.unwrap();
        expect_notifs_10s(
            &mut results,
            HashMap::from([(
                f.test_case(&hash, 1).await,
                vec![TestStatus::Canceled].into(),
            )]),
        )
        .await
        .expect("didn't see test cancellation");

        // They should now get run a second time.
        f.manager
            .set_revisions(vec![hash.clone()].clone())
            .await
            .unwrap();
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("error'd test not re-run"),
            _ = f.scripts[0].started(&hash) => ()
        );
        select!(
            _ = sleep(Duration::from_secs(5)) => panic!("canceled test not re-run"),
            _ = f.scripts[1].started(&hash) => ()
        );
    }
    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
