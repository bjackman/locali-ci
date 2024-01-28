use clap::Parser as _;
use git2;
use std::{error, fmt, process, str};

mod git;

#[derive(Debug)]
enum ErrorKind {
    OpeningRepo,
    GettingHead, // https://www.youtube.com/watch?v=aS8O-F0ICxw
    ParsingBase(String),
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::OpeningRepo => write!(f, "opening repo"),
            ErrorKind::GettingHead => write!(f, "getting head"),
            ErrorKind::ParsingBase(revspec) => write!(f, "parsing base revision {:?}", revspec),
        }
    }
}

#[derive(Debug)]
struct GitError {
    kind: ErrorKind,
    repo_path: String,
    source: git2::Error,
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} for repo {}: {}",
            self.kind, self.repo_path, self.source
        )
    }
}

impl error::Error for GitError {
    fn source(&self) -> Option<&(dyn error::Error + 'static)> {
        Some(&self.source)
    }
}

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

fn do_main() -> Result<(), Box<dyn error::Error>> {
    let args = Args::parse();

    let make_err = |kind| {
        |err| GitError {
            kind,
            repo_path: args.repo_path.to_string(),
            source: err,
        }
    };

    let _ = git::parse_range("foo").unwrap();

    let repo = git2::Repository::open(&args.repo_path).map_err(make_err(ErrorKind::OpeningRepo))?;
    let head = repo.head().map_err(make_err(ErrorKind::GettingHead))?;
    println!("head: {}", str::from_utf8(head.name_bytes()).unwrap());
    let (obj, reference) = repo
        .revparse_ext(&args.base)
        .map_err(make_err(ErrorKind::ParsingBase(args.base)))?;
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
        .spawn()?
        .wait();
    println!("result: {:?}", result);
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{}", e),
    };
}
