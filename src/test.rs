use anyhow::{anyhow, Context};
use cancellation_token::{CancelCallback, CancellationToken, CancellationTokenSource};
use crossbeam_channel;
use nix::errno::Errno;
use nix::sys::signal;
use nix::unistd::{mkdtemp, Pid};
use std::collections;
use std::env;
use std::ffi::OsStr;
use std::os::unix::process::ExitStatusExt;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;

// Manages a bunch of worker threads that run tests for the current set of revisions.
pub struct Manager {
    join_handles: Vec<thread::JoinHandle<anyhow::Result<()>>>,
    chan_tx: crossbeam_channel::Sender<Arc<Task>>,
    // TODO: This is a map from Task::rev -> Task. How can I avoid duplicating the key value?
    queued_tasks: collections::HashMap<String, Arc<Task>>,
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
                    program: program.clone(),
                    args: args.clone(),
                    repo_path: repo_path.clone(),
                    chan_rx: chan_rx.clone(),
                };

                worker.start(cts.token())
            })
            .collect();

        Self {
            join_handles,
            chan_tx,
            queued_tasks: collections::HashMap::new(),
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
        let rev_set: collections::HashSet<&str> = revs.iter().map(|s| s.as_str()).collect();
        for (rev, task) in &self.queued_tasks {
            if !rev_set.contains(rev.as_str()) {
                task.cancel();
            }
        }

        // TODO: skip revs that are already running.
        for rev in revs {
            let task = Arc::new(Task {
                rev: rev.clone(),
                state: Mutex::new(TaskState::NotStarted),
            });
            self.queued_tasks.insert(rev, task.clone());
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
    // Manager. Am I being silly here or is this just hte practical way?
    rev: String,
    state: Mutex<TaskState>,
}

enum TaskState {
    NotStarted,
    Started(Pid),
    Done,
}

impl Task {
    // Attempt to interrupt the task if it is running. If it doesn't work, shrug.
    fn cancel(&self) {
        // TODO: instead of unwrap, do I want to use the thing that continues a prior stacktrace?
        let mut state = self.state.lock().unwrap();
        match *state {
            TaskState::Done => return,
            TaskState::NotStarted => {
                *state = TaskState::Done;
                return;
            }
            TaskState::Started(pid) => {
                match signal::kill(pid, signal::SIGTERM) {
                    Ok(_) => (),
                    Err(Errno::ESRCH) => (), // The process probably just terminated.
                    // TODO logging to a sensible place?
                    Err(errno) => println!("Couldn't kill pid {}: {}", pid, errno.desc()),
                }
                *state = TaskState::Done;
            }
        }
    }

    // Run a command. it will be SIGTERMed if the task is cancelled. It's a bug to call this from
    // two threads at once.
    //
    // TODO: This hard-codes the exact mechanism of spawning and waiting on children. That's no
    // good, because we want to use this for different things (worktree setup, actual tests).
    //
    // TODO: Use the type system to enforce that only one thread can call run_cmd.
    fn run_cmd(&self, cmd: &mut process::Command) -> anyhow::Result<Option<process::ExitStatus>> {
        let mut child;
        {
            let mut state = self.state.lock().unwrap();
            match *state {
                TaskState::Done => return Ok(None), // Already cancelled
                TaskState::Started(_) => panic!("Task started multiple times"),
                TaskState::NotStarted => (),
            };
            child = cmd.spawn().context("spawning child")?;
            *state = TaskState::Started(Pid::from_raw(child.id() as i32));
        }
        let result = child.wait().context("awaiting child").map(|r| Some(r));
        {
            let mut state = self.state.lock().unwrap();
            *state = TaskState::Done
        }
        result
    }
}

// Basically a thread with a lazily-created worktree. Once started, receives Tasks on its channel
// and runs them.
struct Worker {
    id: u32,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager...
    //
    // TODO: Will actually need make these part of the Task.
    program: Arc<String>,
    args: Arc<Vec<String>>,
    repo_path: Arc<String>,
    chan_rx: crossbeam_channel::Receiver<Arc<Task>>,
}

impl Worker {
    fn run_task(
        &mut self,
        worktree_path: &PathBuf,
        task: &Task,
    ) -> anyhow::Result<Option<process::ExitStatus>> {
        // TODO: HOw do I get rid of the &* (to get &str from Arc<String) it looks stupid
        // let worktree = self.get_worktree(&task)?;
        // TODO: join these statements back up again.
        let mut cmd = process::Command::new(&*self.program);
        let cmd = cmd.args(&*self.args);
        let cmd = cmd.current_dir(worktree_path);
        task.run_cmd(cmd)
    }

    // Run a process as a child, block until it's done and return its output, unless canceled.
    // If canceled, SIGINT it, wait for it to complete anyway, but then return None.
    fn run_cmd(
        ct: &CancellationToken,
        cmd: &mut process::Command,
    ) -> anyhow::Result<Option<process::Output>> {
        cmd.stderr(process::Stdio::piped());
        cmd.stdout(process::Stdio::piped());
        let child = cmd.spawn().context("spawning child process")?;
        let pid = Pid::from_raw(child.id() as i32);
        let _ct_reg = ct.register(CancelCallback::FnOnce(Box::new(move || {
            match signal::kill(pid, signal::SIGINT) {
                Ok(_) => (),
                Err(Errno::ESRCH) => (), // The process probably just terminated.
                // TODO logging to a sensible place?
                Err(errno) => println!("Couldn't kill pid {}: {}", pid, errno.desc()),
            }
        })));
        let output = child.wait_with_output().context("awaiting child")?;
        if ct.is_canceled() {
            return Ok(None);
        }
        return Ok(Some(output));
    }

    // run_cmd, then check that the process returned successfully, or return a helpful error.
    // Note you can't tell just from the result whether the operation was canceled.
    // TODO: should this just be done as an extension trait of process::Output?
    fn run_successful_cmd(ct: &CancellationToken, mut cmd: process::Command) -> anyhow::Result<()> {
        match Self::run_cmd(ct, &mut cmd)? {
            None => Ok(()),
            Some(output) => {
                // TODO: Is there a way to write this just using the match?
                if output.status.success() {
                    Ok(())
                } else {
                    Err(match output.status.code() {
                        None => anyhow!(
                            "command {:?} terminated by signal {}",
                            cmd,
                            output.status.signal().unwrap()
                        ),
                        Some(code) => anyhow!(
                            "command {:?} failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                            cmd,
                            code,
                            String::from_utf8_lossy(&output.stderr),
                            String::from_utf8_lossy(&output.stdout)
                        ),
                    })
                }
            }
        }
    }

    fn start(mut self, ct: CancellationToken) -> thread::JoinHandle<anyhow::Result<()>> {
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

            // TODO: How can I avoid this crazy manual OsStr construction?
            let args: Vec<&OsStr> = vec![
                OsStr::new("worktree"),
                OsStr::new("add"),
                path.as_os_str(),
                OsStr::new("HEAD"),
            ];
            let mut cmd = process::Command::new("git");
            cmd.args(args).current_dir(&*self.repo_path);
            Self::run_successful_cmd(&ct, cmd).context("setting up worktree")?;

            for task in self.chan_rx.clone() {
                let result = self.run_task(&path, &task);
                // TODO: Clean up this mess
                println!(
                    "worker {} rev {} -> {:#}",
                    self.id,
                    task.rev,
                    match result {
                        Ok(None) => "canceled".to_string(),
                        Ok(Some(exit_status)) => format!("{}", exit_status),
                        Err(e) => format!("err: {:#}", e),
                    }
                );
            }
            Ok(())
        })
    }
}
