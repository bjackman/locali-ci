use git2;
use std::fmt;

#[derive(Debug)]
enum ErrorKind {
    OpeningRepo,
    GettingHead, // https://www.youtube.com/watch?v=aS8O-F0ICxw
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", match self {
            ErrorKind::OpeningRepo => "opening repo",
            ErrorKind::GettingHead => "getting head",
        })
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
        write!(f, "{} for repo {}: {}", self.kind, self.repo_path, self.source)
    }
}

fn do_main() -> Result<(), GitError> {
    let path = "/home/brendan/src/local-ci";
    let repo = git2::Repository::open(path).map_err(|e| GitError{
        kind: ErrorKind::OpeningRepo, repo_path: path.to_string(), source: e,
    })?;
    let _head = repo.head().map_err(|e| GitError{
        kind: ErrorKind::GettingHead, repo_path: path.to_string(), source: e,
    })?;
    return Ok(());
}

fn main() {
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{}", e),
    };
}