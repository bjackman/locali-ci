use anyhow::Context;
use crossbeam_channel;
use std::panic;
use std::process;
use std::thread;

pub struct Manager<'a> {
    pub num_threads: u32,
    pub current_dir: &'a str,
    pub program: &'a str,
    pub args: &'a Vec<&'a str>,
    chan_tx: crossbeam_channel::Sender<Task>,
    chan_rx: crossbeam_channel::Receiver<Task>,
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

impl<'a> Manager<'a> {
    // TODO: Too many args for a language without keyword args!
    pub fn new(
        num_threads: u32,
        current_dir: &'a str,
        program: &'a str,
        args: &'a Vec<&'a str>,
    ) -> Self {
        let (chan_tx, chan_rx) = crossbeam_channel::unbounded::<Task>();

        Self {
            num_threads,
            current_dir,
            program,
            args,
            chan_tx,
            chan_rx,
        }
    }

    // TODO: implement cancellation.
    pub fn run(&self) {
        thread::scope(|scope| {
            let threads = (1..self.num_threads).map(|i| {
                // TODO: Here I want to move i, but not self. This seems to be exactly what happens.
                // But why?
                scope.spawn(move || self.run_thread(i, self.chan_rx.clone()))
            });
            for t in threads {
                // Thread::join returns an error only when the thread panicked. This is a weird and
                // special error, it doesn't implement error::Error. We just wanna crash the program
                // if we enounter one of those.
                //
                // TODO: In this case the Ok variant of the result is an _inner_ Result which is the
                // value actually returned by the thread function. I dunno what to do with that
                // right now, probably we don't want it, but for the moment I assign it to _.
                let _ = t.join().unwrap_or_else(|e| panic::resume_unwind(e));
            }
        });
    }

    pub fn set_revisions(&self, revs: Vec<String>) {
        // TODO: skip revs that are already running, cancel running revs that are not in @revs.
        for rev in revs {
            // At the moment I think this cannot fail because we never close the receiver. I guess
            // once the lifecycle of the manager is clearer perhaps we want to be able to return an
            // error here?
            self.chan_tx.send(Task{rev}).unwrap();
        }
    }

    fn run_thread(&self, thread_id: u32, chan: crossbeam_channel::Receiver<Task>) -> anyhow::Result<()> {
        for task in chan.iter() {
            process::Command::new(self.program)
                .args(self.args)
                .current_dir(self.current_dir)
                .spawn()
                .with_context(|| format!("execing test executable {:?}", self.program))?
                .wait()
                .context("awaiting test reuslt")?;
            println!("thread {} OK for rev {}", thread_id, task.rev);
        }
        Ok(())
    }
}
