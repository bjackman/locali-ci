use anyhow::Context;
use clap::Parser as _;
use futures::StreamExt;
use std::ffi::OsString;
use std::path::PathBuf;
use std::pin::pin;
use std::str;
use std::sync::Arc;
use tokio::select;

use crate::git::Worktree;

mod config;
mod git;
mod process;
mod resource;
mod test;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo: String,
    /// Maximum number of tests to run concurrently. Each concurrent thread
    /// requires creating a worktree, which is why we don't default to $nproc.
    #[arg(short, long, default_value_t = 8)]
    num_threads: u32,
    /// Command to test. Note this is _not_ run via the shell.
    #[arg(short, long, required = true)]
    config: PathBuf,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    env_logger::init();

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
    };
    // Check repo is valid.
    repo.git_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;
    let repo = Arc::new(repo);
    let mut m = config::create_manager(repo.clone(), &args.config).await?;
    let range_spec: OsString = format!("{}..HEAD", args.base).into();
    m.set_revisions(repo.rev_list(&range_spec).await.context("couldn't rev-list")?);
    let mut revs_stream = repo.watch_refs(&range_spec)?;
    let mut revs_stream = pin!(revs_stream);
    let mut results = m.results();
    loop {
        select!(
            // TODO: It's dumb that we have two different types of communication here (one exposes
            // the channel, one implements Stream).
            revs = revs_stream.next() => {
                // TODO: figure out if/how this can actually fail.
                let revs = revs.expect("revset stream terminated");
                m.set_revisions(revs?);
            },
            result = results.recv() => {
                // https://github.com/rust-lang/futures-rs/issues/1857
                // AFAICS there is no way to encode a stream that never terminates.
                let result = result.expect("result stream terminated");
                // TODO: What the fucking fuck???? I should have used Perl.
                println!("{}", result);
            }
        )
    }
}
