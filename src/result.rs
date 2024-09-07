use std::{
    fs::{self, create_dir_all, File},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

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

    // Prepare to create the output directory for a job output, but don't actually create it yet.
    // It's created once you use one of the methods of CommitOutput for writing data.
    pub fn job_output(&self, hash: &CommitHash, test_name: &str) -> anyhow::Result<CommitOutput> {
        CommitOutput::new(self.base_dir.join(hash.as_ref()).join(test_name))
    }
}

// Output for an individual commit.
pub struct CommitOutput {
    base_dir: PathBuf,
    stdout_opened: bool,
    stderr_opened: bool,
    exit_code_written: bool,
}

impl CommitOutput {
    pub fn new(base_dir: PathBuf) -> anyhow::Result<Self> {
        Ok(Self {
            base_dir,
            stdout_opened: false,
            stderr_opened: false,
            exit_code_written: false,
        })
    }

    // Create and return base directory
    fn get_base_dir(&self) -> Result<&Path> {
        create_dir_all(&self.base_dir).context(format!(
            "creating commit result dir at {}",
            self.base_dir.display()
        ))?;
        Ok(&self.base_dir)
    }

    // Panics if called more than once.
    pub fn stdout(&mut self) -> Result<File> {
        assert!(!self.stdout_opened);
        self.stdout_opened = true;
        Ok(File::create(self.get_base_dir()?.join("stdout.txt"))?)
    }

    // Panics if called more than once.
    pub fn stderr(&mut self) -> Result<File> {
        assert!(!self.stderr_opened);
        self.stderr_opened = true;
        Ok(File::create(self.get_base_dir()?.join("stderr.txt"))?)
    }

    // Note this is called "exitcode" instead of "returncode" because it really
    // only gets set when the child process exits.
    // TODO: Figure out how to record errors in the more general case, probably with a JSON object.
    // Panics if called more than once.
    pub fn set_exit_code(&mut self, exit_code: ExitCode) -> anyhow::Result<()> {
        assert!(!self.exit_code_written);
        self.exit_code_written = true;
        Ok(fs::write(
            self.get_base_dir()?.join("exit_code.txt"),
            format!("{}", exit_code),
        )?)
    }
}

// TODO:
// - Test behaviour on already-existing directories
