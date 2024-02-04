use anyhow::Context;
use clap::Parser as _;
use git2;
use std::collections;
use std::str;

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

fn do_main() -> anyhow::Result<()> {
    let args = Args::parse();

    let repo = git2::Repository::open(&args.repo_path).context("opening repo")?;
    // https://www.youtube.com/watch?v=aS8O-F0ICxw
    let head = repo.head().context("getting head")?;
    println!("head: {}", str::from_utf8(head.name_bytes()).unwrap());
    let (obj, reference) = repo
        .revparse_ext(&args.base)
        .context("parsing base revision")?;
    println!(
        "base: {:?}, {:?}",
        obj,
        reference.map_or("no ref".to_string(), |r| {
            r.kind()
                .map_or("no kind".to_string(), |kind| kind.to_string())
        })
    );

    let mut cmd = collections::VecDeque::from(args.cmd);
    let mut m = test::Manager::new(args.num_threads, cmd.pop_front().unwrap(), Vec::from(cmd));
    m.set_revisions(vec!["HEAD^^".to_string(), "HEAD^".to_string()]);
    m.close();
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{:#}", e),
    };
}
