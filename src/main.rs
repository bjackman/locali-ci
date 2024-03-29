use anyhow::Context;
use clap::Parser as _;
use futures::StreamExt;
use std::collections;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::str;
use tokio;

mod git;
mod process;
mod test;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo_path: String,
    /// Maximum number of tests to run concurrently. Each concurrent thread
    /// requires creating a worktree, which is why we don't default to $nproc.
    #[arg(short, long, default_value_t = 8)]
    num_threads: u32,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
    /// Command to test. Note this is _not_ run via the shell.
    #[arg(trailing_var_arg = true, required = true)]
    cmd: Vec<String>,
}

#[tokio::main]
async fn do_main() -> anyhow::Result<()> {
    let args = Args::parse();

    let repo = git::Repo::open(PathBuf::from(&args.repo_path))
        .context(format!("opening repo {}", args.repo_path))?;
    let mut cmd = collections::VecDeque::from(args.cmd);
    let mut m = test::Manager::new(
        args.num_threads,
        args.repo_path,
        cmd.pop_front().unwrap(),
        Vec::from(cmd),
    );
    let mut revs_stream = repo.watch_refs(&OsStr::new("HEAD^^^..HEAD"))?;
    while let Some(revs) = revs_stream.next().await {
        // TODO: I wrote the manager using proper Strings, oops.
        let revs = revs?
            .into_iter()
            .map(|os_string| os_string.into_string().unwrap())
            .collect();
        m.set_revisions(revs);
    }
    m.close();
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{:#}", e),
    };
}
