use anyhow::Context;
use clap::Parser as _;
use git2;
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

    let m = test::Manager {
        num_threads: args.num_threads,
        current_dir: &args.repo_path,
        program: &args.cmd[0],
        // TODO: How can I avoid this map/as_ref dance? I want to declare test::Manager::args in a
        // way where it doesn't care about the details of the string vec (like how
        // std::Process::Command::args works), but I had borrow checker nightmares.
        args: &args.cmd[1..].iter().map(AsRef::as_ref).collect(),
    };
    println!("{}", m.run()?);
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{:#}", e),
    };
}
