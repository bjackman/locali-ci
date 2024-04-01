use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt as _;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::process::Command as SyncCommand;
use std::time::Duration;

use anyhow::{anyhow, Context};
use async_stream::try_stream;
use futures::{future::Fuse, select, FutureExt, SinkExt as _, StreamExt as _};
use futures_core::{stream::Stream, FusedFuture};
use log::{debug, error};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use tempfile::TempDir;
use tokio::process::Command;
use tokio::time::sleep;

use crate::process::CommandExt;
use crate::process::{OutputExt, SyncCommandExt};

// Worktree represents a git tree, which might be the "main" worktree (in which case it might be
// more clearly refrred to by the name Repo) or some other one.
pub struct Worktree {
    pub path: PathBuf,
}

// Here we don't use the newtype pattern because we actually wanna be able to leak useful features
// of OsString.
pub type RevSpec = OsString;

#[cfg(test)]
pub type CommitHash = String;

impl Worktree {
    pub async fn git_dir(&self) -> anyhow::Result<PathBuf> {
        let mut cmd = Command::new("git");
        let output = (cmd
            .args(["rev-parse", "--git-dir"])
            .current_dir(&self.path)
            .execute()
            .await
            .ok())
        .context("'git rev-parse --git-dir' failed")?;
        Ok(OsStr::from_bytes(&output.stderr).into())
    }

    #[cfg(test)]
    pub async fn init_repo(path: PathBuf) -> anyhow::Result<Self> {
        // TODO: dedupe setting up Command objects
        let mut cmd = Command::new("git");
        cmd.arg("init").current_dir(&path).execute().await?;
        Ok(Self { path })
    }

    #[cfg(test)]
    pub async fn commit(&self, message: &OsStr) -> anyhow::Result<CommitHash> {
        Command::new("git")
            .args(["commit", "-m"])
            .arg(message)
            .current_dir(&self.path)
            .execute()
            .await
            .context("'git commit' failed")?;
        // Doesn't seem like there's a safer way to do this than commit and then retroactively parse
        // HEAD and hope nobody else is messing with us.
        self.rev_parse("HEAD".into()).await
    }

    #[cfg(test)]
    async fn rev_parse(&self, rev_spec: RevSpec) -> anyhow::Result<CommitHash> {
        let stdout = Command::new("git")
            .arg("rev-parse")
            .arg(rev_spec)
            .execute()
            .await
            .context("'git rev-parse' failed")?
            .stdout;
        String::from_utf8(stdout).context("reading git rev-parse output")
    }

    async fn rev_list(&self, range_spec: &OsStr) -> anyhow::Result<Vec<RevSpec>> {
        // TODO: use async command API to support cancellation and avoid blocking.
        let mut cmd = Command::new("git");
        cmd.arg("-C")
            .arg(&self.path)
            .arg("rev-list")
            .arg(range_spec);
        let output = cmd.output().await?;
        // Hack: empirically, rev-list returns 128 when the range is invalid, it's not documented
        // but hopefully this is stable behaviour that we're supposed to be able to rely on for
        // this...?
        if output.code_not_killed()? == 128 {
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
    ) -> anyhow::Result<(
        notify::RecommendedWatcher,
        impl Stream<Item = anyhow::Result<Vec<RevSpec>>> + 'a,
    )> {
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
            .watch(&self.path, RecursiveMode::Recursive)
            .context("setting up watcher")?;

        // This logic "debounces" consecutive events within the same 1s window, to avoid thrashing
        // on the downstream logic as Git works its way through changes.
        Ok((
            watcher,
            try_stream! {
                // Start with an expired timer.
                let mut sleep_fut = pin!(Fuse::terminated());
                loop {
                    select! {
                        // Produce an update when the timer expires.
                        () = sleep_fut =>  yield self.rev_list(range_spec).await?,
                        // Ensure the timer is set when we see an update.
                        maybe_result = rx.next() => {
                            match maybe_result {
                                Some(_result) => {
                                    if sleep_fut.is_terminated() {
                                        sleep_fut.set(sleep(Duration::from_secs(1)).fuse());
                                    }
                                },
                                // TODO: Do I really understand if this can happen? I think maybe not.
                                None  => break,
                            }
                        },
                    }
                }
            },
        ))
    }
}

// A worktree that is deleted when dropped. This is kind of a dumb API that just happens to fit this
// project's exact needs. Instead probably Repo::new and this method should return a common trait or
// something.
pub struct TempWorktree {
    // TODO: It would be nice if we didn't have to own a copy of this PathBuf, but lifetimes are
    // tricky!
    repo_path: PathBuf, // Origin repo
    temp_dir: TempDir,  // Location of worktree
}

impl TempWorktree {
    pub async fn new(repo_path: PathBuf) -> anyhow::Result<Self> {
        // Not doing this async because I assume it's fast, there is no white-glove support, and the
        // drop will have to be synchronous anyway.
        let temp_dir = TempDir::new().context("creating temp dir")?;

        let mut cmd = Command::new("git");
        cmd.args(["worktree", "add"])
            .arg(temp_dir.path())
            .arg("HEAD")
            .execute()
            .await
            .ok()
            .context("setting up worktree")?;

        debug!("Created worktree at {:?}", temp_dir.path());

        Ok(Self {
            repo_path,
            temp_dir,
        })
    }

    pub fn path(&self) -> &Path {
        self.temp_dir.path()
    }
}

impl Drop for TempWorktree {
    fn drop(&mut self) {
        let mut cmd = SyncCommand::new("git");
        cmd.args(["worktree", "remove"])
            .arg(self.temp_dir.path())
            .current_dir(&self.repo_path)
            .execute()
            .unwrap_or_else(|e| {
                error!("Couldn't clean up worktree {:?}: {:?}", &self.temp_dir, e);
            });
        debug!("Delorted worktree at {:?}", self.temp_dir.path());
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
            } else if sb[i] != b'\n' {
                start = i;
                in_line = true;
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
    use std::io::Write;
    use std::fs::File;

    use tempfile::TempDir;

    use super::*;

    #[test_log::test(tokio::test)]
    async fn test_new_gitdir_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        let wt = Worktree {
            path: tmp_dir.path().to_path_buf(),
        };
        assert!(
            wt.git_dir().await.is_err(),
            "opening repo with no .git didn't fail"
        );
    }

    #[test_log::test(tokio::test)]
    async fn test_new_gitdir_file_notgit() {
        let tmp_dir = TempDir::new().expect("couldn't make tempdir");
        {
            let mut bogus_git_file =
                File::create(tmp_dir.path().join(".git")).expect("couldn't create .git");
            write!(bogus_git_file, "no no no").expect("couldn't write .git");
        }
        let wt = Worktree {
            path: tmp_dir.path().to_path_buf(),
        };
        assert!(
            wt.git_dir().await.is_err(),
            "opening repo with bogus .git file didn't fail"
        );
    }
}
