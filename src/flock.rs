// The async-file-lock crate has some issues:
// 1. It doesn't look like it allows upgrades/downgrades of the lock.
// 2. It hasn't been upgraded to tokio 1.0, (I looked briefly at upgrading it but it has a
//    bunch of functionality I don't need or understand, couldn't be bothered).
// 3. I think it leaks threads if you drop the futures it gives you.
//    https://github.com/Stock84-dev/async-file-lock/issues/3

use std::{
    fs::File,
    os::fd::{AsRawFd as _, RawFd},
};

use anyhow::anyhow;

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

pub struct SharedFlock {
    // Hack: just making this public avoids needing to do a bunch of Rustery.
    // This means users can get at the fd and fuck things up if they want, but
    // flocking is kinda fundamentally like that anyway I think (it's not really
    // possible for this library to prevent bugs, since flocks are fundamentally
    // global to the process, not really isolated to the file descriptor).
    pub file: File,
}

impl SharedFlock {
    pub async fn lock(file: File) -> anyhow::Result<Self> {
        flock_async(file.as_raw_fd(), LockKind::Shared).await?;
        Ok(Self { file })
    }

    pub async fn upgrade(self) -> anyhow::Result<ExclusiveFlock> {
        flock_async(self.file.as_raw_fd(), LockKind::Exclusive).await?;
        Ok(ExclusiveFlock { file: self.file })
    }
}

pub struct ExclusiveFlock {
    pub file: File,
}
