use std::{
    fs::{self, create_dir_all, File},
    path::{Path, PathBuf},
};

use anyhow::Context;

use crate::{git::CommitHash, test::ExitCode};

// Result database similar to the design described in
// https://github.com/bjackman/git-brisect?tab=readme-ov-file#the-result-directory
// TODO: Actually we should probably separate it by the repo lol. But how?
pub struct Database {
    base_dir: PathBuf,
}

impl Database {
    pub fn create_or_open(base_dir: &Path) -> anyhow::Result<Self> {
        create_dir_all(base_dir).context(format!(
            "creating result database dir at {}",
            base_dir.display()
        ))?;
        Ok(Self {
            base_dir: base_dir.to_owned(),
        })
    }

    pub fn create_or_open_user() -> anyhow::Result<Self> {
        Self::create_or_open(
            directories::ProjectDirs::from("", "", "local-ci")
                .context("finding user data dir")?
                .data_local_dir(),
        )
    }

    pub fn job_output(&self, hash: &CommitHash, test_name: &str) -> anyhow::Result<CommitOutput> {
        CommitOutput::create(&self.base_dir.join(hash.as_ref()).join(test_name))
    }
}

// Output for an individual commit.
pub struct CommitOutput {
    stdout: Option<File>,
    stderr: Option<File>,
    exit_code_path: PathBuf,
}

impl CommitOutput {
    pub fn create(base_dir: &Path) -> anyhow::Result<Self> {
        create_dir_all(base_dir).context(format!(
            "creating commit result dir at {}",
            base_dir.display()
        ))?;
        Ok(Self {
            stdout: Some(File::create(base_dir.join("stdout.txt"))?),
            stderr: Some(File::create(base_dir.join("stderr.txt"))?),
            exit_code_path: base_dir.join("exit_code.txt"),
        })
    }

    // Returns a value just once.
    pub fn stdout(&mut self) -> Option<File> {
        self.stdout.take()
    }

    // Returns a value just once.
    pub fn stderr(&mut self) -> Option<File> {
        self.stderr.take()
    }

    // Note this is called "exitcode" instead of "returncode" because it really
    // only gets set when the child process exits.
    // TODO: Figure out how to record errors in the more general case, probably with a JSON object.
    pub fn set_exit_code(&self, exit_code: ExitCode) -> anyhow::Result<()> {
        Ok(fs::write(&self.exit_code_path, format!("{}", exit_code))?)
    }
}

// TODO:
// - Test behaviour on already-existing directories