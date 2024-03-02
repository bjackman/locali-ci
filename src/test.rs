use anyhow::Context;
use cancellation_token::{CancellationToken, CancellationTokenSource};
use crossbeam_channel;
use nix::unistd::mkdtemp;
use std::collections;
use std::env;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::Arc;
use std::thread;
use crate::process::CommandExt;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    join_handles: Vec<thread::JoinHandle<anyhow::Result<()>>>,
    chan_tx: crossbeam_channel::Sender<Arc<Task>>,
    task_cts: collections::HashMap<String, CancellationTokenSource>,
    program: Arc<String>,
    args: Arc<Vec<String>>,
}

impl Manager {
    // Starts the workers. You must call close() before dropping it.
    //
    // TODO: Too many args. What are the constructor patterns in Rust? Define a ManagerOpts struct?
    pub fn new(num_threads: u32, repo_path: String, program: String, args: Vec<String>) -> Self {
        let (chan_tx, chan_rx) = crossbeam_channel::unbounded();

        let cts = CancellationTokenSource::new();

        let program = Arc::new(program);
        let args = Arc::new(args);
        let repo_path = Arc::new(repo_path);
        let join_handles = (1..num_threads)
            .map(|i| {
                let worker = Worker {
                    id: i,
                    repo_path: repo_path.clone(),
                    chan_rx: chan_rx.clone(),
                };

                worker.start(cts.token())
            })
            .collect();

        Self {
            join_handles,
            chan_tx,
            task_cts: collections::HashMap::new(),
            program,
            args,
        }
    }

    // TODO: should I mandate that this gets called? I guess I could just do it in Drop but it feels
    // like a lot of work for an implicit call. I do note that nothing mandates you join threads.
    //
    // Oh: https://doc.rust-lang.org/book/ch20-03-graceful-shutdown-and-cleanup.html seems like
    // doing it in Drop would be pretty standard.
    pub fn close(self) {
        for jh in self.join_handles {
            let _ = jh.join().unwrap_or_else(|e| panic::resume_unwind(e));
        }
    }

    // Interrupt any revisions that are not in revs, start testing all revisions in revs that are
    // not already tested or being tested.
    pub fn set_revisions(&mut self, revs: Vec<String>) {
        let mut rev_set: collections::HashSet<&str> = revs.iter().map(|s| s.as_str()).collect();
        for (rev, cts) in &self.task_cts {
            // We're already testing rev, so we don't need to kick it off below.
            if !rev_set.remove(rev.as_str()){
                // This rev is being tested but wasn't in rev_set.
                cts.cancel();
            }
        }

        for rev in rev_set {
            let cts = CancellationTokenSource::new();
            let task = Arc::new(Task {
                ct: cts.token(),
                rev: rev.to_string(),
                program: self.program.clone(),
                args: self.args.clone(),
            });
            self.task_cts.insert(rev.to_string(), cts);
            // At the moment I think this cannot fail because we never close the receiver. I guess
            // once the lifecycle of the manager is clearer perhaps we want to be able to return an
            // error here?
            self.chan_tx.send(task).unwrap();
        }
    }
}

// Work item to test a specific revision, that can be cancelled. This is unfornately coupled with
// the assumption that the task is actually a subprocess, which sucks.
struct Task {
    // We just denote the revision as a string and not a stronger type because revisions can
    // disappear anyway.
    //
    // TODO: I made this a String instead of &str, because the lifetime of the str would need to be
    // the lifetime of the task - but that lifetime is not really visible to the user of the
    // Manager. Am I being silly here or is this just the practical way?
    rev: String,
    ct: CancellationToken,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager (basically same comment as for
    // .rev)
    program: Arc<String>,
    args: Arc<Vec<String>>,
}

impl Task {
    fn run(&self, worktree: &PathBuf) -> anyhow::Result<process::Output> {
        let mut checkout_cmd = process::Command::new("git");
        checkout_cmd
            .args(["checkout", &self.rev])
            .current_dir(worktree)
            .output_ok(&self.ct)?;

        let mut cmd = process::Command::new(&*self.program);
        // TODO: Is that stupid-looking &* a smell that I'm doing something stupid?
        cmd.args(&*self.args).current_dir(worktree);
        cmd.output_ct(&self.ct)
    }
}

// Basically a thread with a lazily-created worktree. Once started, receives Tasks on its channel
// and runs them.
struct Worker {
    id: u32,
    repo_path: Arc<String>,
    chan_rx: crossbeam_channel::Receiver<Arc<Task>>,
}

impl Worker {
    fn start(self, ct: CancellationToken) -> thread::JoinHandle<anyhow::Result<()>> {
        thread::spawn(move || {
            // First create a worktree for the worker. This is slow so it should be cancelable. git2
            // doesn't support canceling this operation, which is fine, the git CLI is a perfectly
            // cromulent API anyway.
            //
            // I originally also had this on-demand only when receiving the first task. I'm not
            // sure, maybe that was a better approach since it avoids unnecessary work, but
            // ultimately this project is supposed to be about getting the user their test results
            // sooner. So the sooner we kick off the worktree setup the quickker we can run the
            // tests when the first requests come through (which, until we implement some storage
            // cache for results, is always gonna be immediately on startup anyway)
            let path = mkdtemp(&env::temp_dir().join("local-ci-XXXXXX"))
                .context("mkdtemp for worktree")?;

            let mut cmd = process::Command::new("git");
            cmd.args(["worktree", "add"])
                .arg(&path)
                .arg("HEAD")
                .current_dir(&*self.repo_path)
                .output_ok(&ct)
                .context("setting up worktree")?;

            for task in self.chan_rx.clone() {
                let result = task.run(&path);
                // TODO: Clean up this mess
                println!(
                    "worker {} rev {} -> {:#}",
                    self.id,
                    task.rev,
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
