use anyhow::Context;
use crossbeam_channel;
use nix::errno::Errno;
use nix::sys::signal;
use nix::unistd::Pid;
use std::collections;
use std::panic;
use std::process;
use std::sync::{Arc, Mutex};
use std::thread;

pub struct Manager {
    join_handles: Vec<thread::JoinHandle<()>>,
    chan_tx: crossbeam_channel::Sender<Arc<Task>>,
    // TODO: This is a map from Task::rev -> Task. How can I avoid duplicating the key value?
    queued_tasks: collections::HashMap<String, Arc<Task>>,
}

impl Manager {
    pub fn new(num_threads: u32, repo_path: &str, program: String, args: Vec<String>) -> Self {
        let (chan_tx, chan_rx) = crossbeam_channel::unbounded();

        let program = Arc::new(program);
        let args = Arc::new(args);
        let join_handles = (1..num_threads)
            .map(|i| {
                let worker = Worker {
                    id: i,
                    program: program.clone(),
                    args: args.clone(),
                    current_dir: repo_path.to_string(),
                    chan_rx: chan_rx.clone(),
                };

                worker.start()
            })
            .collect();

        Self {
            join_handles,
            chan_tx,
            queued_tasks: collections::HashMap::new(),
        }
    }

    // TODO: How can I mandate that this is called? I guess I could just do it in Drop but it feels
    // like a lot of work for an implicit call. I do note that nothing mandates you join threads.
    pub fn close(self) {
        for jh in self.join_handles {
            jh.join().unwrap_or_else(|e| panic::resume_unwind(e));
        }
    }

    pub fn set_revisions(&mut self, revs: Vec<String>) {
        let rev_set: collections::HashSet<&str> = revs.iter().map(|s| s.as_str()).collect();
        for (rev, task) in &self.queued_tasks {
            if !rev_set.contains(rev.as_str()) {
                task.cancel();
            }
        }

        // TODO: skip revs that are already running, cancel running revs that are not in @revs.
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

impl Task {
    fn cancel(&self) {
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

enum TaskState {
    NotStarted,
    Started(Pid),
    Done,
}

struct Worker {
    id: u32,
    // TODO: This incurs an atomic operation on setup/shutdown. But presumably there is a way to
    // just make these references to a value owned by the Manager...
    program: Arc<String>,
    args: Arc<Vec<String>>,
    current_dir: String,
    chan_rx: crossbeam_channel::Receiver<Arc<Task>>,
}

impl Worker {
    fn run_task(&self, task: &Task) -> anyhow::Result<Option<process::ExitStatus>> {
        // TODO: HOw do I get rid of the &* (to get &str from Arc<String) it looks stupid
        task.run_cmd(
            process::Command::new(&*self.program)
                .args(&*self.args)
                .current_dir(&self.current_dir),
        )
    }

    fn start(self) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            for task in self.chan_rx.clone() {
                let result = self.run_task(&task);
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
        })
    }
}
