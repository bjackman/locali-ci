use std::collections;
use std::collections::HashMap;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;

use anyhow::Context;
// Tokio's multiple-consumer channels only support broadcast where each receiver gets every message.
use async_channel;
use futures::future::join_all;
use futures::StreamExt;
use nix::unistd::mkdtemp;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::git::RevSpec;
use crate::process::CommandExt;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    job_cts: HashMap<RevSpec, CancellationToken>,
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
    chan_tx: async_channel::Sender<Job>,
    join_handles: Vec<JoinHandle<anyhow::Result<()>>>,
}

impl Manager {
    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: Too many args. What are the constructor patterns in Rust? Define a ManagerOpts struct?
    // TODO: Get rid of String
    pub async fn new(
        num_threads: u32,
        repo_path: String,
        program: OsString,
        args: Vec<OsString>,
    ) -> Self {
        let (chan_tx, chan_rx) = async_channel::unbounded();
        let repo_path = Arc::new(repo_path);
        let join_handles = join_all(
            (1..num_threads)
                .map(|i| {
                    let worker = Worker {
                        id: i,
                        repo_path: repo_path.clone(),
                        chan_rx: chan_rx.clone(),
                    };

                    worker.start()
                })).await;
                // .collect::<Vec<Future<Output = JoinHandle<anyhow::Result<()>>>>>(),

        Self {
            job_cts: HashMap::new(),
            program: Arc::new(program),
            args: Arc::new(args),
            join_handles,
            chan_tx,
        }
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    pub async fn set_revisions(&mut self, revs: Vec<RevSpec>) {
        let mut rev_set: collections::HashSet<&OsStr> =
            revs.iter().map(|s| s.as_os_str()).collect();
        for (rev, ct) in &self.job_cts {
            // We're already testing rev, so we don't need to kick it off below.
            if !rev_set.remove(rev.as_os_str()) {
                // This rev is being tested but wasn't in rev_set.
                ct.cancel();
            }
        }

        for rev in rev_set {
            let ct = CancellationToken::new();
            self.job_cts.insert(rev.to_os_string(), ct.clone());
            let job = Job {
                ct: ct.clone(),
                rev: rev.to_os_string(),
                program: self.program.clone(),
                args: self.args.clone(),
            };
            // At the moment I think this cannot fail because we never close the receiver. I guess
            // once the lifecycle of the manager is clearer perhaps we want to be able to return an
            // error here?
            self.chan_tx.send(job).await.unwrap();
        }
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
    ct: CancellationToken,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager (basically same comment as for
    // .rev)
    program: Arc<OsString>,
    args: Arc<Vec<OsString>>,
}

impl Job {
    async fn run(&self, worktree: &PathBuf) -> anyhow::Result<std::process::Output> {
        let mut checkout_cmd = Command::new("git");
        checkout_cmd
            .arg("checkout")
            .arg(&self.rev)
            .current_dir(worktree)
            .output_ok()
            .await?;

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
    repo_path: Arc<String>,
    chan_rx: async_channel::Receiver<Job>,
}

impl Worker {
    async fn start(self) -> JoinHandle<anyhow::Result<()>> {
        tokio::spawn(async move {
            // TODO: Make async
            let path = mkdtemp(&env::temp_dir().join("local-ci-XXXXXX"))
                .context("mkdtemp for worktree")?;

            let mut cmd = Command::new("git");
            cmd.args(["worktree", "add"])
                .arg(&path)
                .arg("HEAD")
                .current_dir(&*self.repo_path)
                .output_ok()
                .await
                .context("setting up worktree")?;

            let mut rx = pin!(self.chan_rx);
            while let Some(job) = rx.next().await {
                let result = job.run(&path);
                // TODO: Clean up this mess
                println!(
                    "worker {} rev {:?} -> {:#}",
                    self.id,
                    job.rev,
                    match result.await {
                        Ok(output) => format!("{:?}", output),
                        Err(e) => format!("err: {:#}", e),
                    }
                );
            }
            Ok(())
        })
    }
}
