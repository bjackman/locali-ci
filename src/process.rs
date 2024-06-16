use anyhow::{anyhow, Context};
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt as _;
use std::process::{Command as SyncCommand, Output};
use tokio::process::Command;

pub trait OutputExt {
    // Returns exit code, fails verbosely if the process was killed by a signal.
    fn code_not_killed(&self) -> anyhow::Result<i32>;
    // Fails verbosely unless the command completed successfully
    fn ok(&self) -> anyhow::Result<()>;
}

impl OutputExt for Output {
    fn code_not_killed(&self) -> anyhow::Result<i32> {
        self.status.code().ok_or_else(|| {
            anyhow!(
                "terminated by signal {}",
                self.status
                    .signal()
                    .expect("ExitStatus::code() and ExitStatus::signal() both None")
            )
        })
    }

    fn ok(&self) -> anyhow::Result<()> {
        match self.code_not_killed()? {
            0 => Ok(()),
            code => Err(anyhow!(
                "failed with exit code {:?}. stderr:\n{:?}\nstdout:\n{:?}",
                code,
                OsStr::from_bytes(&self.stderr),
                OsStr::from_bytes(&self.stdout),
            )),
        }
    }
}

pub trait CommandExt {
    // Run a command and fail informatively if anything at all goes wrong.
    async fn execute(&mut self) -> anyhow::Result<Output>;
}

impl CommandExt for Command {
    async fn execute(&mut self) -> anyhow::Result<Output> {
        let output = self.output().await.context("couldn't run command")?;
        output.ok()?;
        Ok(output)
    }
}

pub trait SyncCommandExt {
    fn execute(&mut self) -> anyhow::Result<()>;
}

impl SyncCommandExt for SyncCommand {
    fn execute(&mut self) -> anyhow::Result<()> {
        self.output().context("couldn't run command")?.ok()
    }
}
