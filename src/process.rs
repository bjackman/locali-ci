use anyhow::anyhow;
use std::error;
use std::fmt;
use std::os::unix::process::ExitStatusExt;
use std::process;

// TODO: This feels a bit like spooky action at a distance, maybe it woud be better to just have
// plain old functions for this. But it is quite nice to have the usage look fairly harmonic with
// normal process API usage. Since this is partly a learning project, I'll do the fancy thing and
// if it turns out to be too clever I'll have learned a lesson.
pub trait CommandExt {
    // Like the above, but fails if the process is terminated by a signal.
    async fn output_not_killed(&mut self) -> anyhow::Result<process::Output>;
    // Like the above, but also fails if the process exits with a non-zero return code.
    // This is a convenience hack, somewhat like
    // std::process::ExitStatus::exit_ok, but it's more informative. Arguably we
    // should have a ExitStatusExt for that rather than just squashing it into CommandExt.
    async fn output_ok(&mut self) -> anyhow::Result<()>;
}

// cancellation_token::Canceled doesn't implement std::errorr::Error so we can't put it into an
// anyhow error. Here's a custom error type that we can.
//
// TODO: Maybe it's dumb to squash cancellation status into the error band. Should we nest
// MayBeCanceled with Result, or just return an Option which is None under cancellation? That would
// be pretty un-ergonomic in cases where we don't expect cancellation though.=
#[derive(Debug)]
pub struct Canceled {}

impl fmt::Display for Canceled {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        // Write strictly the first element into the supplied output
        // stream: `f`. Returns `fmt::Result` which indicates whether the
        // operation succeeded or failed. Note that `write!` uses syntax which
        // is very similar to `println!`.
        write!(f, "canceled")
    }
}

impl error::Error for Canceled {
    // If we integrated properly with the cancellation library it would be cool to report the token
    // hierarchy here.
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        None
    }
    fn description(&self) -> &str {
        "token canceled"
    }
    fn cause(&self) -> Option<&dyn error::Error> {
        None
    }
}

impl CommandExt for tokio::process::Command {
    // Returns the Output, but fails if the child was killed by a signal.
    async fn output_not_killed(&mut self) -> anyhow::Result<process::Output> {
        let output = self.output().await?;
        match output.status.code() {
            None => Err(anyhow!(
                "terminated by signal {}",
                output
                    .status
                    .signal()
                    .expect("ExitStatus::code() and ExitStatus::signal() both None")
            )),
            Some(_) => Ok(output),
        }
    }

    async fn output_ok(&mut self) -> anyhow::Result<()> {
        let output = self.output_not_killed().await?;
        match output.status.code().unwrap() {
            0 => Ok(()),
            code => Err(anyhow!(
                "failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                code,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            )),
        }
    }
}
