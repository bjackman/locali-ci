use anyhow::Context;
use clap::Parser as _;
use futures::StreamExt;
use std::collections;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::pin::pin;
use std::str;

mod git;
mod process;
mod test;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf

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
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    env_logger::init();

    let repo = git::Repo::open(PathBuf::from(&args.repo_path))
        .context(format!("opening repo {}", args.repo_path))?;
    let mut cmd = collections::VecDeque::from(args.cmd);
    let mut m = test::Manager::new(
        args.num_threads,
        args.repo_path.as_ref(),
        OsString::from(cmd.pop_front().unwrap()),
        cmd.iter().map(OsString::from).collect(),
    ).await;
    let (_watcher, mut revs_stream) = repo.watch_refs(OsStr::new("HEAD^^^..HEAD"))?;
    let mut revs_stream = pin!(revs_stream);
    while let Some(revs) = revs_stream.next().await {
        println!("update");
        let revs = revs?
            .into_iter()
            .collect();
        m.set_revisions(revs).await?;
    }
    println!("revset stream terminated");
    Ok(())
}
