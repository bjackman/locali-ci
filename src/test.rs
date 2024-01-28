use anyhow::Context;
use std::process;
use std::thread;

pub struct Manager<'a> {
    pub num_threads: u32,
    pub current_dir: &'a str,
    pub program: &'a str,
    pub args: &'a Vec<&'a str>,
}

impl<'a> Manager<'a> {
    // TODO: implement cancellation.
    pub fn run(&self) -> anyhow::Result<()> {
        thread::scope(|scope| {
            for i in 0..self.num_threads {
                // TODO: Here I want to move i, but not self. This seems to be exactly what happens.
                // But why?
                scope.spawn(move || self.run_thread(i));
            }
        });

        // // Thread::join returns an error only when the thread panicked. This is a weird and special
        // // error, it doesn't implement error::Error.
        // t.join().unwrap_or_else(|e| { panic::resume_unwind(e) });
        Ok(())
    }

    pub fn run_thread(&self, thread_id: u32) -> anyhow::Result<()> {
        process::Command::new(self.program)
            .args(self.args)
            .current_dir(self.current_dir)
            .spawn()
            .with_context(|| format!("execing test executable {:?}", self.program))?
            .wait()
            .context("awaiting test reuslt")?;
        println!("thread {} OK", thread_id);
        Ok(())
    }
}
