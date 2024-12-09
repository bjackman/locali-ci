use std::{
    fs::{create_dir_all, File, OpenOptions},
    io::{Read as _, Seek as _, Write as _},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
#[allow(unused_imports)]
use log::debug;
use serde::{Deserialize, Serialize};

use crate::{
    flock::{ExclusiveFlock, SharedFlock},
    git::Hash,
    test::{ConfigHash, TestCase, TestJobOutput, TestName, TestResult},
};

// Result database similar to the design described in
// https://github.com/bjackman/git-brisect?tab=readme-ov-file#the-result-directory
// TODO: Actually we should probably separate it by the repo lol. But how?
pub struct Database {
    pub base_dir: PathBuf,
}

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
struct TestResultEntry {
    config_hash: ConfigHash,
    result: TestResult,
}

pub enum LookupResult {
    FoundResult(DatabaseEntry),
    YouRunIt(DatabaseOutput),
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

    pub fn result_relpath(test_case: &TestCase) -> PathBuf {
        Path::new(test_case.storage_hash()).join(&test_case.test.name)
    }

    fn result_path(&self, hash: &Hash, test_name: &TestName) -> PathBuf {
        self.base_dir.join::<&str>(hash.as_ref()).join(test_name)
    }

    // Either get or create a result in the database. If there's a test running,
    // this blocks until it's done. If it returns YouRunIt, then you get to
    // write via the provided output into the database. In that case, you will
    // block anyonne else trying to get the entry until you drop the DatabaseOutput.
    pub async fn lookup(&self, test_case: &TestCase) -> Result<LookupResult> {
        let result_dir = self.result_path(test_case.storage_hash(), &test_case.test.name);
        create_dir_all(&result_dir)
            .with_context(|| format!("creating commit result dir at {}", result_dir.display()))?;
        let json_path = result_dir.join("result.json");
        let json_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(result_dir.join("result.json"))
            .context("opening result JSON")?;

        // First we lock the file for reading so we can check if there's a result in there already.
        // This will block if someone else is running the test.
        let mut flock = SharedFlock::lock(json_file)
            .await
            .context("locking JSON file for reading")?;

        let mut json = String::new();
        flock
            .file
            .read_to_string(&mut json)
            .context("reading JSON")?;
        if !json.is_empty() {
            match serde_json::from_str::<TestResultEntry>(&json) {
                Ok(test_result) => {
                    // Has the configuration changed? if not we need to rerun regardless.
                    if test_result.config_hash == test_case.test.config_hash {
                        // Was the test configured to accept cached results?
                        if test_case.cache_hash.is_some() {
                            // Cool, we're done.
                            return Ok(LookupResult::FoundResult(DatabaseEntry {
                                base_path: result_dir.clone(),
                                result: test_result,
                            }));
                        }
                    }
                }
                Err(e) => {
                    // This probably just means limmat got killed before we finished
                    // writing the result.
                    debug!(
                        "Error reading result JSON from {}: {e} - JSON\n{:?}",
                        json_path.display(),
                        json,
                    );
                }
            }
        }
        // Gotta rewind as well as truncate, otherwise the file is padded with 0s.
        flock.file.rewind().context("rewinding JSON file")?;
        flock.file.set_len(0).context("truncating JSON file")?;

        // We have to run the test. Upgrade the lock to exclusive.
        let flock = flock.upgrade().await.context("upgrading JSON file lock")?;

        Ok(LookupResult::YouRunIt(
            DatabaseOutput::new(result_dir, test_case.test.config_hash, flock)
                .context("creating database entry")?,
        ))
    }
}

// Existing entry in the database.
#[derive(Debug)]
pub struct DatabaseEntry {
    base_path: PathBuf,
    result: TestResultEntry,
}

impl DatabaseEntry {
    pub fn result(&self) -> &TestResult {
        &self.result.result
    }

    pub fn stdout_path(&self) -> PathBuf {
        self.base_path.join("stdout.txt")
    }

    pub fn stderr_path(&self) -> PathBuf {
        self.base_path.join("stderr.txt")
    }
}

// Output for an individual test job, stored into the database
pub struct DatabaseOutput {
    base_dir: PathBuf, // Must exist.
    stdout_opened: bool,
    stderr_opened: bool,
    status_written: bool,
    config_hash: ConfigHash,
    json_flock: ExclusiveFlock,
}

impl DatabaseOutput {
    pub fn new(
        base_dir: PathBuf,
        config_hash: ConfigHash,
        json_flock: ExclusiveFlock,
    ) -> anyhow::Result<Self> {
        debug!("Creating database entry at {base_dir:?}");
        Ok(Self {
            base_dir,
            stdout_opened: false,
            stderr_opened: false,
            status_written: false,
            config_hash,
            json_flock,
        })
    }
}

impl TestJobOutput for DatabaseOutput {
    type Stream = File;

    fn stdout(&mut self) -> Result<File> {
        assert!(!self.stdout_opened);
        self.stdout_opened = true;
        let path = self.base_dir.join("stdout.txt");
        File::create(&path).with_context(|| format!("creating {}", path.display()))
    }

    fn stderr(&mut self) -> Result<File> {
        assert!(!self.stderr_opened);
        self.stderr_opened = true;
        let path = self.base_dir.join("stderr.txt");
        File::create(&path).with_context(|| format!("creating {}", path.display()))
    }

    // TODO: Figure out how to record errors in the more general case, probably with a JSON object.
    fn set_result(&mut self, result: &TestResult) -> anyhow::Result<()> {
        assert!(!self.status_written);
        self.status_written = true;
        let entry = TestResultEntry {
            config_hash: self.config_hash,
            result: result.clone(),
        };
        self.json_flock
            .file
            .write_all(&serde_json::to_vec(&entry).expect("failed to serialize TestStatus"))
            .context("writing JSON result")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tempfile::TempDir;

    use crate::{git::Commit, test::Test};

    use super::*;

    #[test_log::test(tokio::test)]
    async fn test_corrupted_result() {
        let db_dir = TempDir::new().unwrap();
        let db = Database::create_or_open(db_dir.path()).unwrap();

        // Setup: Create a corrupted database entry. This simulates Limmat
        // getting killed in the middle of writing.
        // Best way to make sure we are corrupting data that the database will
        // really is to have the database write it in the first place.
        let test_case = TestCase::new(Commit::arbitrary(), Arc::new(Test::arbitrary()));
        let json_path = {
            let mut output = match db.lookup(&test_case).await.unwrap() {
                LookupResult::FoundResult(_) => panic!("Found result in empty database"),
                LookupResult::YouRunIt(output) => output,
            };
            output
                .stderr()
                .unwrap()
                .write_all(b"hello stderr\n")
                .unwrap();
            output
                .stdout()
                .unwrap()
                .write_all(b"hello stdout\n")
                .unwrap();
            output.set_result(&TestResult { exit_code: 1 }).unwrap();
            output.base_dir.join("result.json")
        };
        {
            let mut f = OpenOptions::new()
                .write(true)
                .open(json_path)
                .expect("couldn't open JSON file");
            for _ in 0..16 {
                f.write_all(b"I DON`T TIHKN THA'TS JASON BATMAN,,,\n")
                    .unwrap();
            }
        }

        // Act: Now open the entry. It should just silently act as though nothing was there.
        {
            let mut output = match db.lookup(&test_case).await.unwrap() {
                LookupResult::FoundResult(e) => panic!("successfully read corrupted JSON? {e:?}"),
                LookupResult::YouRunIt(output) => output,
            };
            output.set_result(&TestResult { exit_code: 2 }).unwrap();
        }
        // Now it should be valid again.
        match db.lookup(&test_case).await.unwrap() {
            LookupResult::FoundResult(entry) => assert_eq!(entry.result.result.exit_code, 2),
            LookupResult::YouRunIt(_) => panic!("no JSON found after DB corruption"),
        };
    }
}
// TODO:
// - Test behaviour on already-existing directories
