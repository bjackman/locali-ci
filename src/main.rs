use anyhow::Context;
use clap::Parser as _;
use git2;
use std::{process, str};

mod git;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo_path: String,
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

    let _ = git::parse_range("foo").unwrap();

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

    let result = process::Command::new(&args.cmd[0])
        .args(&args.cmd[1..])
        .current_dir(&args.repo_path)
        .spawn()
        .with_context(|| format!("execing test executable {:?}", args.cmd[0]))?
        .wait();
    println!("result: {:?}", result);
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{:#}", e),
    };
}
