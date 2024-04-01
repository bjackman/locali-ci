use anyhow::anyhow;
use std::os::unix::process::ExitStatusExt as _;
use std::process::Output;

pub trait OutputExt {
    // Returns exit code, fails verbosely if the process was killed by a signal.
    fn code_not_killed(&self) -> anyhow::Result<i32>;
    // Fails verbosely unless the command completed successfully
    // TODO: Is ok a bad name for this? Clashes with Option::ok.
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
                self.stderr,
                self.stdout,
            )),
        }
    }
}
