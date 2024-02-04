use anyhow::Context;
use crossbeam_channel;
use std::collections;
use std::process;
use std::panic;
use std::sync::Arc;
use std::thread;

pub struct Manager {
    join_handles: Vec<thread::JoinHandle<()>>,
    chan_tx: crossbeam_channel::Sender<Arc<Task>>,
    // TODO: This is a map from Task::rev -> Task. How can I avoid duplicating the key value?
    queued_tasks: collections::HashMap<String, Arc<Task>>,
}

struct Task {
    // We just denote the revision as a string and not a stronger type because revisions can
    // disappear anyway.
    //
    // TODO: I made this a String instead of &str, because the lifetime of the str would need to be
    // the lifetime of the task - but that lifetime is not really visible to the user of the
    // Manager. Am I being silly here or is this just hte practical way?
    rev: String,
}

impl Manager {
    pub fn new(num_threads: u32, program: String, args: Vec<String>) -> Self {
        let (chan_tx, chan_rx) = crossbeam_channel::unbounded();

        let program = Arc::new(program);
        let args = Arc::new(args);
        let join_handles = (1..num_threads)
            .map(|i| {
                let worker = Worker {
                    id: i,
                    program: program.clone(),
                    args: args.clone(),
                    current_dir: "foo".to_string(),
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
        // TODO: skip revs that are already running, cancel running revs that are not in @revs.
        for rev in revs {
            let task = Arc::new(Task { rev: rev.clone() });
            self.queued_tasks.insert(rev, task.clone());
            // At the moment I think this cannot fail because we never close the receiver. I guess
            // once the lifecycle of the manager is clearer perhaps we want to be able to return an
            // error here?
            self.chan_tx.send(task).unwrap();
        }
    }
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
    fn start(self) -> thread::JoinHandle<()> {
        thread::spawn(move || self.run())
    }

    // TODO: Implement cancellation
    fn run(&self) {
        for task in self.chan_rx.clone() {
            process::Command::new(&*self.program)
                .args(&*self.args)
                .current_dir(&self.current_dir)
                .spawn()
                // TODO: remove unwrap
                .with_context(|| format!("execing test executable {:?}", self.program)).unwrap()
                .wait()
                // TODO: remove unwrap
                .context("awaiting test reuslt").unwrap();
            println!("worker {} OK for rev {}", self.id, task.rev);
        }
    }
}
