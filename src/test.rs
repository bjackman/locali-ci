use std::borrow::Borrow;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ffi::OsString;
use std::path::Path;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use futures::future::{FutureExt, join_all, select_all, SelectAll};
use futures::StreamExt;
use log::{info, warn};
use nix::sys::signal::kill;
use nix::sys::signal::Signal;
use nix::unistd::Pid;
use tokio::process::Command;
use tokio::select;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::git::{CommitHash, Worktree};
use crate::process::OutputExt;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    job_cts: HashMap<CommitHash, CancellationToken>,
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
    chan_tx: async_channel::Sender<Job>,
    worker_shutdown: SelectAll<JoinHandle<anyhow::Result<()>>>,
}

impl Manager {
    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: Too many args. What are the constructor patterns in Rust? Define a ManagerOpts struct?
    pub async fn new<W>(num_threads: u32, repo: W, program: OsString, args: Vec<OsString>) -> Self
    where
        // TODO: is this just a really silly roundabout way of saying that W has to be
        // Arc<Worktree>?
        W: Borrow<Worktree> + Send + Clone + 'static,
    {
        let (chan_tx, chan_rx) = async_channel::unbounded();
        let join_handles = join_all((0..num_threads).map(|i| {
            let worker = Worker {
                id: i,
                chan_rx: chan_rx.clone(),
            };

            worker.start(repo.clone())
        }))
        .await;

        Self {
            job_cts: HashMap::new(),
            program: Arc::new(program),
            args: Arc::new(args),
            worker_shutdown: select_all(join_handles),
            chan_tx,
        }
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    pub async fn set_revisions(&mut self, revs: Vec<CommitHash>) -> anyhow::Result<()> {
        let mut to_start = HashSet::<&CommitHash>::from_iter(revs.iter());
        let mut cancel_revs = Vec::new();
        for rev in self.job_cts.keys() {
            // We're already testing rev, so we don't need to kick it off below.
            if !to_start.remove(rev) {
                // This rev is being tested but wasn't in rev_set.
                cancel_revs.push(rev)
            }
        }
        info!("Starting {:?}, cancelling {:?}", to_start, cancel_revs);
        for rev in cancel_revs {
            self.job_cts[rev].cancel();
        }

        for rev in to_start {
            let ct = CancellationToken::new();
            self.job_cts.insert((*rev).clone(), ct.clone());
            let job = Job {
                ct: ct.clone(),
                rev: (*rev).clone(),
                program: self.program.clone(),
                args: self.args.clone(),
            };
            // Send the job down the channel, but bail if any of the workers are dead.
            //
            // TODO: This is a dreadful mess. There are too many layers of fallbility, maybe I am
            // doing something wrong.
            select!(
                (result, _, _) = &mut self.worker_shutdown => {
                    result.expect("select failed").context("worker thread shut down")
                },
                result = self.chan_tx.send(job) => {
                    result.expect("channel send failed");
                    Ok(())
                }
            )?
        }
        Ok(())
    }
}

// Work item to test a specific revision, that can be cancelled. This is unfornately coupled with
// the assumption that the task is actually a subprocess, which sucks.
struct Job {
    // We just denote the revision as a string and not a stronger type because revisions can
    // disappear anyway.
    //
    // TODO: I made this a String instead of &str, because the lifetime of the str would need to be
    // the lifetime of the task - but that lifetime is not really visible to the user of the
    // Manager. Am I being silly here or is this just the practical way?
    rev: CommitHash,
    // TODO: Implement cancellation.
    ct: CancellationToken,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager (basically same comment as for
    // .rev)
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
}

impl Job {
    // Returns None if the job is cancelled, once it has been killed.
    async fn run(&self, worktree: &Path) -> anyhow::Result<Option<std::process::Output>> {
        // TODO: Move this logic into the git module.
        let mut checkout_cmd = Command::new("git");
        (checkout_cmd
            .arg("checkout")
            .arg(&self.rev)
            .current_dir(worktree)
            .output()
            .await?
            .ok())
        .context(format!(
            "checking out revision {:?} in {:?}",
            self.rev, worktree
        ))?;

        let mut cmd = Command::new(&*self.program);
        // TODO: Is that stupid-looking &* a smell that I'm doing something stupid?
        cmd.args(&*self.args).current_dir(worktree);
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
        // We'll interrupt the child if we get cancelled, but we must wait for it to finish
        // regardless before we can release the worktree for another job.
        // TODO: this whole dance seems pretty ridiculous, would be interesting
        // to come back to this with some more experience...
        let mut wait_fut = pin!(child.wait_with_output());
        let mut cancel_fut = pin!(self.ct.cancelled().fuse());
        let mut cancelled = false;
        // Max 2 iterations are possible.
        loop {
            select! {
                result = &mut wait_fut => {
                    // I think maybe a true Rustacean would write this block as a
                    // single chain of methods? But it seems ridiculous to me.
                    let output = result.map_err(anyhow::Error::from)?;
                    if cancelled {
                        // Don't care about actual outcome of job.
                        return Ok(None);
                    } else {
                        return Ok(Some(output))
                    }
                },
                _ = &mut cancel_fut => {
                    kill(pid, Signal::SIGINT).context("couldn't interrupt child job")?;
                    cancelled = true;
                }
            }
        }
    }
}

// Basically a thread with a lazily-created worktree. Once started, receives Tasks on its channel
// and runs them.
struct Worker {
    id: u32,
    // Tokio's multiple-consumer channels only support broadcast where each receiver gets every
    // message.
    chan_rx: async_channel::Receiver<Job>,
}

// TODO: Now that we have async, this is a dumb architecture. Instead of spinning up a task per
// worktree and then feeding jobs into them, we should just create a task immediately when we create
// a job, and it should just block until it can get an available worktree from a pool.
impl Worker {
    // TODO: Need to log somewhere immediately when the worker hits an irrecoverable error.
    async fn start<W>(self, repo: W) -> JoinHandle<anyhow::Result<()>>
    where
        W: Borrow<Worktree> + Send + 'static,
    {
        // TODO: How can I get the repo path into the worker task more cleanly than this? If we had
        // an Rc or Arc we could clone that. Can we do that without having to hard-code the smart
        // pointer type in Manager::new, perhaps using Borrow<Worktree>?
        tokio::spawn(async move {
            // TODO: Where do we handle failure of this?
            let worktree = repo
                .borrow()
                .temp_worktree()
                .await
                .context("setting up worker")
                .map_err(|e| {
                    warn!("{:#}", e);
                    e
                })?;

            info!(
                "Worker {} started, working in {:?}",
                self.id,
                worktree.path()
            );

            let mut rx = pin!(self.chan_rx);
            while let Some(job) = rx.next().await {
                let result = job.run(worktree.path()).await;
                // TODO: Clean up this mess
                info!(
                    "worker {} rev {:?} -> {:#}",
                    self.id,
                    job.rev,
                    match result {
                        Ok(output) => format!("{:?}", output),
                        Err(e) => format!("err: {:#}", e),
                    }
                );
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tempfile::TempDir;
    use test_log;
    use tokio::{
        fs,
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
        repo: Arc<Worktree>,
    }

    impl Fixture {
        async fn new() -> Self {
            let temp_dir = TempDir::with_prefix("fixture-").expect("couldn't make tempdir");
            let repo = Worktree::init_repo(temp_dir.path().into())
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
        Never,
    }

    impl TestScript {
        const STARTED_FILENAME_PREFIX: &'static str = "started.";

        // Creates a script, this will create a temporary directory, which will
        // be destroyed on drop.
        fn new(terminate: Terminate) -> Self {
            let dir = TempDir::with_prefix("test-script-").expect("couldn't make tempdir");
            let script = format!(
                "touch {:?}$(git rev-parse HEAD) {}",
                dir.path().join(Self::STARTED_FILENAME_PREFIX),
                match terminate {
                    Terminate::Immediately => "",
                    Terminate::Never => "&& read",
                }
            );
            Self {
                dir,
                script: script.into(),
            }
        }

        // Pass this to Manager::new
        fn program(&self) -> OsString {
            "bash".into()
        }
        // Pass this to Manager::new
        fn args(&self) -> Vec<OsString> {
            vec!["-xc".into(), self.script.clone()]
        }

        // Blocks until the script is started for the given commit hash.
        async fn started(&self, hash: &CommitHash) {
            // Argh I dunno this is annoying.
            let mut filename = OsString::from(Self::STARTED_FILENAME_PREFIX);
            filename.push(hash.as_ref());
            let path = self.dir.path().join(filename);
            await_exists_and_read(path).await;
        }
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
        let mut m = Manager::new(2, fixture.repo.clone(), script.program(), script.args()).await;
        m.set_revisions(vec![hash.clone()])
            .await
            .expect("couldn't set_revisions");
        // TODO: Instead of watching until we see the command being done, ask the manager when it's
        // stable.
        select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s"),
            _ = script.started(&hash) => (),
        );
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let fixture = Fixture::new().await;
        let hash1 = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let script = TestScript::new(Terminate::Never);
        let mut m = Manager::new(2, fixture.repo.clone(), script.program(), script.args()).await;
        m.set_revisions(vec![hash1.clone()])
            .await
            .expect("couldn't set_revisions");
        select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s for hash1"),
            _ = script.started(&hash1) => ()
        );
        let hash2 = fixture
            .repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        m.set_revisions(vec![hash2.clone()])
            .await
            .expect("couldn't set_revisions");
        select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s for hash2"),
            _ = script.started(&hash2) => ()
        );
        // TODO: check that the old test gets aborted.
    }

    // TODO: test cancellation
    // TODO: test starting up on an empty repo?
    // TODO: test only one worker task (I think this is actually broken)
    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
