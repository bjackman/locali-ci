// The async-file-lock crate has some issues:
// 1. It doesn't look like it allows upgrades/downgrades of the lock.
// 2. It hasn't been upgraded to tokio 1.0, (I looked briefly at upgrading it but it has a
//    bunch of functionality I don't need or understand, couldn't be bothered).
// 3. I think it leaks threads if you drop the futures it gives you.
//    https://github.com/Stock84-dev/async-file-lock/issues/3

use std::{
    fs::File,
    io::{Read as _, Seek as _, Write as _},
    os::fd::{AsRawFd as _, RawFd},
};

use anyhow::{anyhow, Context as _};

use nix::{
    errno::Errno,
    libc::{self, LOCK_EX, LOCK_SH},
};
use tokio::task::{self};

#[derive(Debug)]
enum LockKind {
    Shared,
    Exclusive,
}

impl LockKind {
    fn flock_arg(&self) -> i32 {
        match self {
            Self::Shared => LOCK_SH,
            Self::Exclusive => LOCK_EX,
        }
    }
}

fn flock(fd: RawFd, kind: LockKind) -> anyhow::Result<()> {
    let res = unsafe { libc::flock(fd, kind.flock_arg()) };
    Errno::result(res)
        .map(drop)
        .map_err(|errno| anyhow!("flock({kind:?} failed: {errno}"))
}

// It's key that this takes a RawFd and not an OwnedFd or File or whatever: we
// musn't move the file into the task, since we want it to be closed if the
// future using this function gets dropped. This is also why we are forced to
// use the raw libc flock, since the nix Flock API expects to take ownership of
// the file.
async fn flock_async(fd: RawFd, kind: LockKind) -> anyhow::Result<()> {
    task::spawn_blocking(move || flock(fd, kind)).await.unwrap()
}

// A simple "read" lock on a file.
#[derive(Debug)]
pub struct SharedFlock {
    file: File,
    content: String,
}

impl SharedFlock {
    // Lock an open file, this also immediately reads the whole content which
    // can access via `content`. The file should be freshly-opened.
    pub async fn new(mut file: File) -> anyhow::Result<Self> {
        flock_async(file.as_raw_fd(), LockKind::Shared).await?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .context("reading locked file")?;
        Ok(Self { file, content })
    }

    // The content of the file.
    // This returns a reference to reflect the fact that the validity of the
    // content is tied to the lifetime of the lock.
    pub fn content(&self) -> &str {
        &self.content
    }

    // Upgrade to a "write" lock. This is not an atomic operation, when you do
    // this the content of the file can change, which is reflected by the fact
    // that the reference returned by `content` is invalid now, so you should
    // check the `content` of the result again.
    pub async fn upgrade(mut self) -> anyhow::Result<ExclusiveFlock> {
        self.file.rewind().context("rewinding locked file")?;
        ExclusiveFlock::new(self.file).await
    }
}

// A simple "write" lock on a file.
pub struct ExclusiveFlock {
    file: File,
    content: String,
}

impl ExclusiveFlock {
    async fn new(mut file: File) -> anyhow::Result<Self> {
        debug_assert_eq!(file.stream_position().unwrap(), 0);
        flock_async(file.as_raw_fd(), LockKind::Exclusive).await?;
        let mut content = String::new();
        file.read_to_string(&mut content)
            .context("reading locked")?;
        file.rewind().context("rewinding locked file")?;
        Ok(Self { file, content })
    }

    pub fn content(&self) -> &str {
        &self.content
    }

    // Replace the content of the file.
    pub fn set_content(&mut self, content: &[u8]) -> anyhow::Result<()> {
        debug_assert_eq!(self.file.stream_position().unwrap(), 0);
        self.file.set_len(0).context("truncating locked file")?;
        self.file
            .write_all(content)
            .context("writing locked file")?;
        // TODO: it would be nicer if this method consumed self, then we wouldn't have to rewind.
        self.file.rewind().context("rewinding locked file")
    }
}
