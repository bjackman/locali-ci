use anyhow::{anyhow, Context};

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

// This module contains horribly manual git logic. This is manual for two main reasons:
// - We need to be able to get notified of changes to ranges, this is not something that git
//   natively supports so we actually need to peek at .git.
// - We want cancellation on operations like checkout, since that can take some time on large repos.
//   The Git CLI supports this but libraries don't. The Git CLI is actually Git's only properly
//   supported "API" anyway I believe.

pub struct Repo {
    git_dir: PathBuf,
}

impl Repo {
    pub fn open(path: PathBuf) -> anyhow::Result<Self> {
        let mut git_file = File::open(&path.join(".git"))?;
        if git_file.metadata()?.file_type().is_dir() {
            return Ok(Repo { git_dir: path });
        }

        fn strip_newline(b: &[u8]) -> &[u8] {
            b.strip_suffix("\n".as_bytes()).unwrap_or(b)
        }

        // .git is not a directory. Is it a worktree pointer? That's a file that looks like
        // "gitdir: /path/to/gitdir"
        let mut content = Vec::new();
        git_file.read_to_end(&mut content)?;
        let path = match content.strip_prefix("gitdir: ".as_bytes()) {
            None => return Err(anyhow!(".git text file didn't start with 'gitdir: '")),
            Some(suffix) => PathBuf::from(OsStr::from_bytes(strip_newline(suffix))),
        };
        // It should be a subdir of the original .git dir, named "worktrees/$name".
        let worktrees_path = path.parent().ok_or(anyhow!(format!(
            "{:?} not a worktree path (no parent)",
            path
        )))?;
        if worktrees_path.file_name() != Some(&OsStr::new("worktrees")) {
            return Err(anyhow!(format!("{:?} not a worktrees path", path)))?;
        }
        let git_path = worktrees_path.parent().ok_or(anyhow!(format!(
            "{:?} not a worktree path (no parent)",
            path
        )))?;
        let git_file = File::open(&git_path).context(format!("open worktree origin {:?}", path))?;
        if !git_file.metadata()?.file_type().is_dir() {
            return Err(anyhow!(format!("not a git repository: {:?}", path)));
        }
        Ok(Repo {
            git_dir: PathBuf::from(git_path),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::process::CommandExt;
    use cancellation_token::CancellationToken;
    use std::io::Write;
    use std::path::Path;
    use std::process;
    use tempfile::TempDir;

    #[test]
    fn test_new_gitdir_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        assert!(
            Repo::open(tmp_dir.path().to_path_buf()).is_err(),
            "opening repo with no .git didn't fail"
        );
    }

    #[test]
    fn test_new_gitdir_file_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        {
            let mut bogus_git_file =
                File::create(tmp_dir.path().join(".git")).expect("couldn't create .git");
            write!(bogus_git_file, "no no no").expect("couldn't write .git");
        }
        assert!(
            Repo::open(tmp_dir.path().to_path_buf()).is_err(),
            "opening repo with bogus .git file didn't fail"
        );
    }

    fn must_git<I, S>(path: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = process::Command::new("git");
        cmd.arg("-C").arg(path).args(args);
        cmd.output_ok(&CancellationToken::new(false))
            .expect("git command failed");
    }

    #[test]
    fn test_new_gitdir() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        must_git(tmp_dir.path(), ["init"]);
        let repo = Repo::open(tmp_dir.path().to_path_buf()).expect("failed to open repo");
        assert_eq!(repo.git_dir, tmp_dir.path());
    }

    #[test]
    fn test_new_gitdir_worktree() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        let worktree = TempDir::new().expect("couldn't make worktree tempdir");
        println!("tmp_dir {:?} worktree {:?}", tmp_dir, worktree);
        must_git(tmp_dir.path(), ["init"]);
        must_git(tmp_dir.path(), ["commit", "--allow-empty", "-m", "foo"]);
        let mut cmd = process::Command::new("git");
        cmd.arg("-C")
            .arg(tmp_dir.path())
            .args(["worktree", "add"])
            .arg(worktree.path())
            .arg("HEAD");
        cmd.output_ok(&CancellationToken::new(false))
            .expect("couldn't setup git worktree");
        let repo = Repo::open(worktree.path().to_path_buf()).expect("failed to open repo");
        assert_eq!(repo.git_dir, tmp_dir.path().join(".git"));
    }
}
