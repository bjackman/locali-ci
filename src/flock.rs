// The async-file-lock crate has some issues:
//
// 1. It doesn't look like it allows upgrades/downgrades of the lock.
// 2. It hasn't been upgraded to tokio 1.0, (I looked briefly at upgrading it but it has a
//    bunch of functionality I don't need or understand, couldn't be bothered).
// 3. I think it leaks threads if you drop the futures it gives you.
//    https://github.com/Stock84-dev/async-file-lock/issues/3
//
// This is a very simple flock library that is not really generic, it serves the
// rather specific needs of using small files kinda like "database entries".

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs::File,
    io::{Read as _, Seek as _, Write as _},
    os::fd::{AsRawFd as _, RawFd},
};

use anyhow::{anyhow, Context as _};

use nix::{
    errno::Errno,
    libc::{self, EWOULDBLOCK, LOCK_EX, LOCK_NB, LOCK_SH},
};
use tokio::task::{self};

#[derive(Debug)]
enum LockKind {
    TryShared,
    Shared,
    Exclusive,
}

impl LockKind {
    fn flock_arg(&self) -> i32 {
        match self {
            Self::TryShared => LOCK_NB | LOCK_SH,
            Self::Shared => LOCK_SH,
            Self::Exclusive => LOCK_EX,
        }
    }
}

#[derive(Debug)]
enum FlockError {
    WouldBlock,
    Other(anyhow::Error),
}

impl Display for FlockError {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        match self {
            Self::WouldBlock => write!(f, "would block"),
            Self::Other(e) => write!(f, "{}", e),
        }
    }
}

impl Error for FlockError {}

fn flock(fd: RawFd, kind: LockKind) -> Result<(), FlockError> {
    let res = unsafe { libc::flock(fd, kind.flock_arg()) };
    match res {
        EWOULDBLOCK => Err(FlockError::WouldBlock),
        errno => Errno::result(errno)
            .map(drop)
            .map_err(|errno| FlockError::Other(anyhow!("flock({kind:?} failed: {errno}"))),
    }
}

// It's key that this takes a RawFd and not an OwnedFd or File or whatever: we
// musn't move the file into the task, since we want it to be closed if the
// future using this function gets dropped. This is also why we are forced to
// use the raw libc flock, since the nix Flock API expects to take ownership of
// the file.
async fn flock_async(fd: RawFd, kind: LockKind) -> Result<(), FlockError> {
    task::spawn_blocking(move || flock(fd, kind)).await.unwrap()
}

#[derive(Debug)]
pub struct SharedFlock {
    file: File,
    content: String,
}

impl SharedFlock {
    fn new_inner(mut file: File) -> anyhow::Result<Self> {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .context("reading locked file")?;
        Ok(Self { file, content })
    }

    // Lock an open file, this also immediately reads the whole content which
    // can access via `content`. The file should be freshly-opened.
    pub async fn new(file: File) -> anyhow::Result<Self> {
        flock_async(file.as_raw_fd(), LockKind::Shared)
            .await
            .context("calling flock")?;
        Self::new_inner(file)
    }

    // Like new, but returns None if the file is exclusive-locked.
    pub async fn try_new(file: File) -> anyhow::Result<Option<Self>> {
        match flock_async(file.as_raw_fd(), LockKind::TryShared).await {
            Err(FlockError::WouldBlock) => return Ok(None),
            Err(FlockError::Other(err)) => return Err(err),
            Ok(_) => (),
        };
        Ok(Some(Self::new_inner(file)?))
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
    // Even though this is a "write lock" in a sense, the file needs to be open for reading too.
    pub async fn new(mut file: File) -> anyhow::Result<Self> {
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

    // See SharedFlock::upgrade - same limiations apply.
    pub async fn downgrade(mut self) -> anyhow::Result<SharedFlock> {
        self.file.rewind().context("rewinding locked file")?;
        SharedFlock::new(self.file).await
    }
}
