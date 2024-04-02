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
    pub async fn new(
        num_threads: u32,
        repo: &Worktree,
        program: OsString,
        args: Vec<OsString>,
    ) -> Self {
        let (chan_tx, chan_rx) = async_channel::unbounded();
        let join_handles = join_all((1..num_threads).map(|i| {
            let worker = Worker {
                id: i,
                chan_rx: chan_rx.clone(),
            };

            worker.start(repo)
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

impl Worker {
    // TODO: Need to log somewhere immediately when the worker hits an irrecoverable error.
    async fn start(self, repo: &Worktree) -> JoinHandle<anyhow::Result<()>> {
        // TODO: How can I get the repo path into the worker task more cleanly than this? If we had
        // an Rc or Arc we could clone that. Can we do that without having to hard-code the smart
        // pointer type in Manager::new, perhaps using Borrow<Worktree>?
        let repo_path = repo.path.clone();
        tokio::spawn(async move {
            // TODO: Where do we handle failure of this?
            let repo = Worktree { path: repo_path };
            let worktree = repo
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
    use tokio::time::{interval, sleep};

    use crate::git::Worktree;

    use super::*;

    // Blocks until file exists, the dumb way.
    async fn file_exists(path: &Path) {
        let mut interval = interval(Duration::from_millis(10));
        while !path.try_exists().unwrap() {
            interval.tick().await;
        }
    }

    #[test_log::test(tokio::test)]
    // TODO: this doesn't actually test cancellation lmao
    async fn test_cancellation() {
        let temp_dir = TempDir::new().expect("couldn't make tempdir");
        let repo = Worktree::init_repo(temp_dir.path().into())
            .await
            .expect("couldn't init test repo");
        // TODO: check correct version was tested.
        let _hash = repo
            .commit("hello,".as_ref())
            .await
            .expect("couldn't create test commit");
        let started_path = temp_dir.path().join("started");
        let script = format!("touch {}", started_path.to_string_lossy());
        let mut m = Manager::new(2, &repo, "bash".into(), vec!["-c".into(), script.into()]).await;
        m.set_revisions(vec!["HEAD".into()])
            .await
            .expect("couldn't set_revisions");
        // TODO: Instead of watching until we see the command being done, ask the manager when it's
        // stable.
        select!(
            _ = sleep(Duration::from_secs(1)) => panic!("script did not run after 1s"),
            _ = file_exists(&started_path) => (),
        )
    }

    // TODO: test starting up on an empty repo?
}
