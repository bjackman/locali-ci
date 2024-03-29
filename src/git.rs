use anyhow::{anyhow, Context};
use async_stream::try_stream;
use cancellation_token::CancellationTokenSource;
use futures::{SinkExt as _, StreamExt as _};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};

use crate::process::CommandExt;

use futures_core::stream::Stream;

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::unix::ffi::OsStrExt as _;
use std::path::PathBuf;
use std::process;

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
        let mut git_file = File::open(&path.join(".git")).context("opening .git")?;
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

    fn rev_list(&self, range_spec: &OsStr) -> anyhow::Result<Vec<OsString>> {
        // TODO: use async command API to support cancellation and avoid blocking.
        let mut cmd = process::Command::new("git");
        cmd.arg("-C")
            .arg(&self.git_dir)
            .arg("rev-list")
            .arg(range_spec);
        let cts = CancellationTokenSource::new();
        let output = cmd.output_not_killed(&cts.token())?;
        // Hack: empirically, rev-list returns 128 when the range is invalid, it's not documented
        // but hopefully this is stable behaviour that we're supposed to be able to rely on for
        // this...?
        if output.status.code().unwrap() == 128 {
            return Ok(vec![]);
        }
        let code = output.status.code().unwrap();
        if code != 0 {
            return Err(anyhow!(
                "failed with exit code {}. stderr:\n{}\nstdout:\n{}",
                code,
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            ));
        }
        Ok(OsStr::from_bytes(&output.stdout)
            .split_lines()
            .iter()
            // TODO: How do I avoid allocating and copying each string here? I
            // think I need to move the stdout into the caller, is there an
            // ergonomic way to do that?
            .map(|os_str| os_str.to_os_string())
            .collect())
    }

    // Watch for events that could change the meaning of a revspec. When that happens, send an event
    // on the channel with the new resolved spec.
    //
    // TODO: How do I hide the notify::RecommendedWatcher from the caller? They need to own it
    // because otherwise it just gets dropped. I think I probably want to just move it into the
    // object I return that implements Stream.
    pub fn watch_refs<'a>(
        &'a self,
        range_spec: &'a OsStr,
    ) -> anyhow::Result<(notify::RecommendedWatcher, impl Stream<Item = anyhow::Result<Vec<OsString>>> + 'a)> {
        // Alternatives considered/attempted:
        //
        // - inotify (also fanotify) doesn't support recursively watching directories, whereas the
        //   notify crate has convenient support for that.
        // - The notify crate has convenient support for sending stuff directly down std::sync::mpsc
        //   channels, and even has support for debouncing those events. However it seems like you
        //   then need to create a whole additional channel if you wanna "map" the events to
        //   something else, i.e. like we wannaho call rev_list here.
        //
        // Overall the idea of how to turn this into an async thingy comes from
        // https://github.com/notify-rs/notify/blob/main/examples/async_monitor.rs, I am not sure if
        // this is "real" or toy code that I should not have followed so literally, in particular I
        // am not clear on the implications of taking the async send operation and just wrapping it
        // in futures::executor::block_on. I think I could also use the debouncer even with the
        // async approach but I think then there would be a lot of unnecessary logic going on under
        // the hood, like I guess it probably spins up a thread.
        let (mut tx, mut rx) = futures::channel::mpsc::unbounded();

        // Automatically select the best implementation for your platform.
        // You can also access each implementation directly e.g. INotifyWatcher.
        let mut watcher = RecommendedWatcher::new(
            move |res| {
                futures::executor::block_on(async {
                    // TODO: I am not sure what I am unwrapping here, when does the send fail?
                    tx.send(res).await.unwrap();
                })
            },
            Config::default(),
        )?;
        watcher
            .watch(&self.git_dir, RecursiveMode::Recursive)
            .context("setting up watcher")?;
        Ok((watcher, try_stream! {
            while let Some(result) = rx.next().await {
                result?;
                yield self.rev_list(range_spec)?;
            }
            println!("watcher done");
        }))
    }
}

trait OsStrExt {
    fn split_lines(&self) -> Vec<&OsStr>;
}

impl OsStrExt for OsStr {
    fn split_lines(&self) -> Vec<&OsStr> {
        let mut start = 0;
        let mut ret = vec![];
        let sb = self.as_bytes();
        let mut in_line = sb[0] != b'\n';
        // How do i wrote code?
        for i in 1..sb.len() {
            if in_line {
                if sb[i] == b'\n' {
                    ret.push(OsStr::from_bytes(&sb[start..i]));
                    in_line = false;
                }
            } else {
                if sb[i] != b'\n' {
                    start = i;
                    in_line = true;
                }
            }
        }
        if in_line {
            ret.push(OsStr::from_bytes(&sb[start..sb.len()]));
        }
        ret
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
