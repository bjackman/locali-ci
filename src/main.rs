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
    // TODO: Is there a nice way to make these error constructions more concise?
    // Possibly by redesigning the error types?
    let repo = git2::Repository::open(path).map_err(|e| GitError{
        kind: ErrorKind::OpeningRepo, repo_path: path.to_string(), source: e,
    })?;
    let _head = repo.head().map_err(|e| GitError{
        kind: ErrorKind::GettingHead, repo_path: path.to_string(), source: e,
    })?;
    return Ok(());
}

fn main() {
    // TODO: I found if I just return a Result from main, it doesn't use Display
    // it just debug-prints the struct. So here I"m just manually printing the
    // Display representation. Is there a smarter way to do this?
    match do_main() {
        Ok(()) => println!("OK!"),
        Err(e) => println!("{}", e),
    };
}