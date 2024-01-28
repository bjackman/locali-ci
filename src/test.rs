use anyhow::Context;
use std::process;

pub struct Manager<'a>
{
    pub num_threads: u32,
    pub current_dir: &'a str,
    pub program: &'a str,
    pub args: &'a Vec<&'a str>,
}

impl<'a> Manager<'a> {
    pub fn run(&self) -> anyhow::Result<process::ExitStatus> {
        process::Command::new(self.program)
            .args(self.args)
            .current_dir(self.current_dir)
            .spawn()
            .with_context(|| format!("execing test executable {:?}", self.program))?
            .wait()
            .context("awaiting test reuslt")
    }
}
