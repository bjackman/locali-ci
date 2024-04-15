use std::borrow::Borrow;
use std::collections;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::pin::pin;
use std::sync::Arc;

use anyhow::Context;
use futures::future::{join_all, select_all, SelectAll};
use futures::StreamExt;
use log::{info, warn};
use tokio::process::Command;
use tokio::select;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::git::{RevSpec, Worktree};
use crate::process::OutputExt;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    job_cts: HashMap<RevSpec, CancellationToken>,
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
        let join_handles = join_all((1..num_threads).map(|i| {
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
    pub async fn set_revisions(&mut self, revs: Vec<RevSpec>) -> anyhow::Result<()> {
        let mut rev_set: collections::HashSet<&OsStr> =
            revs.iter().map(|s| s.as_os_str()).collect();
        let mut cancel_revs = Vec::new();
        for rev in self.job_cts.keys() {
            // We're already testing rev, so we don't need to kick it off below.
            if !rev_set.remove(rev.as_os_str()) {
                // This rev is being tested but wasn't in rev_set.
                cancel_revs.push(rev)
            }
        }
        info!("Starting {:?}, cancelling {:?}", rev_set, cancel_revs);
        for rev in cancel_revs {
            self.job_cts[rev].cancel();
        }

        for rev in rev_set {
            let ct = CancellationToken::new();
            self.job_cts.insert(rev.to_os_string(), ct.clone());
            let job = Job {
                _ct: ct.clone(),
                rev: rev.to_os_string(),
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
    rev: RevSpec,
    // TODO: Implement cancellation.
    _ct: CancellationToken,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager (basically same comment as for
    // .rev)
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
}

impl Job {
    async fn run(&self, worktree: &Path) -> anyhow::Result<std::process::Output> {
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
        cmd.output().await.map_err(anyhow::Error::from)
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

    use crate::git::Worktree;

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
    struct Fixture(TempDir, Arc<Worktree>);

    impl Fixture {
        async fn new() -> Self {
            let temp_dir = TempDir::with_prefix("fixture-").expect("couldn't make tempdir");
            let repo = Worktree::init_repo(temp_dir.path().into())
                .await
                .expect("couldn't init test repo");
            Self(temp_dir, Arc::new(repo))
        }
    }

    #[test_log::test(tokio::test)]
    async fn should_run_single() {
        let Fixture(temp_dir, repo) = Fixture::new().await;
        let hash = repo
            .commit("hello,")
            .await
            .expect("couldn't create test commit");
        let hash_path = temp_dir.path().join("hash");
        let script = format!("git rev-parse HEAD >> {}", hash_path.to_string_lossy());
        let mut m = Manager::new(
            2,
            repo,
            "bash".into(),
            vec!["-c".into(), script.into()],
        )
        .await;
        m.set_revisions(vec!["HEAD".into()])
            .await
            .expect("couldn't set_revisions");
        // TODO: Instead of watching until we see the command being done, ask the manager when it's
        // stable.
        let got_hash = select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s"),
            content = await_exists_and_read(&hash_path) => content,
        );
        assert_eq!(got_hash, hash);
    }

    #[test_log::test(tokio::test)]
    async fn should_cancel_running() {
        let Fixture(temp_dir, repo) = Fixture::new().await;
        repo.commit("hello,")
            .await
            .expect("couldn't create test commit");
        // This script will block forever.
        let script = format!(
            "cd {} && touch started.$(git rev-parse HEAD) && read",
            temp_dir.path().to_string_lossy()
        );
        let mut m = Manager::new(
            2,
            repo.clone(),
            "bash".into(),
            // TODO: make this more generic so we don't need the .into()
            vec!["-c".into(), script.into()],
        )
        .await;
        m.set_revisions(vec!["HEAD".into()])
            .await
            .expect("couldn't set_revisions");
        // Wait until the script has definitely started.
        let started_path = temp_dir.path().join(format!(
            "started.{}",
            repo.rev_parse("HEAD".into()).await.unwrap()
        ));
        select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s"),
            _ = await_exists_and_read(started_path) => ()
        );
        // TODO: check that the new commit starts getting tested.
        // TODO: check that the old test gets aborted.
    }

    // TODO: test cancellation
    // TODO: test starting up on an empty repo?
    // TODO: test only one worker task (I think this is actually broken)
    // TODO: if the tests fail, the TempWorktree cleanup goes haywire, something
    // to do with panic and drop order I think.
}
