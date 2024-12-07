use std::{collections::HashMap, ffi::OsStr, io::Write, mem, sync::Arc};

use ansi_control_codes::control_sequences::{CUP, ED};
use anyhow::{self, bail, Context as _};
use colored::Colorize;
use lazy_static::lazy_static;
use regex::Regex;

use crate::{
    database::Database,
    git::{CommitHash, Worktree},
    http::UiState,
    test::{Notification, TestCase, TestInconclusive, TestName, TestStatus},
    text::{Class, Line, Span, Text},
    util::{Rect, ResultExt as _},
};

struct TrackedTestCase {
    test_case: TestCase,
    status: TestStatus,
}

// Inner string key is test name. Here we awkwardly store this as a
// two-level map instead of a flat one by TestCaseId, because that
// conveniently lets us grab all the TestCases for a given commit when
// rendering the output.
type TrackedCases = HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>;

// Updates the awkward nested hashmap to reflect a new notification coming in.
// Standalone function for convenient use in tests.
fn update_tracked_cases(tracked_cases: &mut TrackedCases, notif: Arc<Notification>) {
    let commit_statuses = tracked_cases
        .entry(notif.test_case.commit_hash.clone())
        .or_default();
    commit_statuses.insert(
        notif.test_case.test.name.clone(),
        TrackedTestCase {
            test_case: notif.test_case.clone(),
            status: notif.status.clone(),
        },
    );
}

// Tracks the status of the tests being run by observing the notification
// stream.
pub struct StatusTracker<W: Worktree, O: Write> {
    repo: Arc<W>,
    tracked_cases: TrackedCases,
    output_buf: OutputBuffer,
    output: O,
    web_ui: Arc<UiState>,
    result_url_base: String,
    home_url: String,
}

// This ought to be private to Tracker::reset, rust just doesn't seem to let you do that.
lazy_static! {
    static ref COMMIT_HASH_REGEX: Regex = Regex::new("[0-9a-z]{40,}").unwrap();
    static ref GRAPH_COMPONENT_REGEX: Regex = Regex::new(r"[\\/\*]").unwrap();
}

impl<W: Worktree, O: Write> StatusTracker<W, O> {
    // Construct a tracker that will write the UI to the given outut. The URL
    // base is used to generate hyperlinks to test results.
    pub fn new(
        repo: Arc<W>,
        output: O,
        web_ui: Arc<UiState>,
        result_url_base: impl Into<String>,
        home_url: impl Into<String>,
    ) -> Self {
        Self {
            repo,
            tracked_cases: HashMap::new(),
            output_buf: OutputBuffer::empty(),
            output,
            web_ui,
            result_url_base: result_url_base.into(),
            home_url: home_url.into(),
        }
    }

    // Informs the tracker of the range of tests that we expect to be testing.
    pub async fn set_range(&mut self, range_spec: &OsStr) -> anyhow::Result<()> {
        // This should eventually be configurable.
        let log_format =
            "%Cred%h%Creset -%C(yellow)%d%Creset %s %Cgreen(%cr) %C(bold blue)<%an>%Creset";

        self.output_buf = OutputBuffer::new(&self.repo, range_spec, log_format).await?;
        Ok(())
    }

    // Absorb a notification.
    pub fn update(&mut self, notif: Arc<Notification>) {
        update_tracked_cases(&mut self.tracked_cases, notif);
    }

    // Update the UI by writing it to the output with fancy terminal escape
    // codes to overwrite what was previously written.
    pub fn repaint(&mut self, term_size: &Rect) -> anyhow::Result<()> {
        let render = self
            .output_buf
            .render(&self.tracked_cases, &self.result_url_base)?;

        self.web_ui.set_log_buf(render.html_pre());

        // Enter alternate screen. Dunno why ansi-control-codes doesn't have
        // this. This isn't really how I wanted this UI to work. But
        // implementing what I really wanted turns out to be really fucking
        // fiddly and unsatisfying and boring, I just don't care enough.
        // This is idempotent so we just do it every time.
        write!(self.output, "\x1B[?1049h")?;
        // Move cursor to top left and erase the display.
        write!(&mut self.output, "{}{}", CUP(Some(0), Some(0)), ED(None))?;
        let truncated = Text::from_iter(
            render
                .into_lines()
                .take(term_size.rows)
                .map(|l| l.truncate_graphemes(term_size.cols)),
        );
        write!(&mut self.output, "{}", truncated.ansi())?;
        writeln!(
            &mut self.output,
            "Web UI: {}",
            self.home_url.bold().on_blue()
        )?;

        Ok(())
    }
}

impl<W: Worktree, O: Write> Drop for StatusTracker<W, O> {
    fn drop(&mut self) {
        write!(self.output, "\x1B[?1049h").or_log_error("Couldn't exit alternate screen");
    }
}

// Represents the buffer showing the current status of all the commits being tested.
struct OutputBuffer {
    // Pre-rendered lines containing static information (graph, commit log info etc).
    lines: Vec<String>,
    // lines[i] should be appended with the live status information of tests for status_commit[i].
    status_commits: HashMap<usize, CommitHash>,
}

impl OutputBuffer {
    pub fn empty() -> Self {
        Self {
            lines: Vec::new(),
            status_commits: HashMap::new(),
        }
    }

    pub async fn new<W: Worktree, S: AsRef<OsStr>>(
        repo: &Arc<W>,
        range_spec: S,
        log_format: &str,
    ) -> anyhow::Result<Self> {
        // All right this is gonna seem pretty hacky. We're gonna get the --graph log
        // as a text blob, then we're gonna use our pre-existing knowledge about
        // its contents as position anchors to patch it with the information we need.
        // This saves us having to actually write any algorithms ourselves. Basically
        // we only care about the structure of the DAG in so far as it influences the layout
        // of characters we're gonna display in the terminal. So, we just get
        // Git to tell us that exact information 🤷.
        // This is actually the same approach taken by the code I looked at in
        // the edamagit VSCode extension.
        // Note it's tricky because, even if you simplify it by fixing the
        // number of lines that the non-graph section of the output occupies,
        // the graph logic can still sometimes occupy more more lines when
        // history is very complex.
        //
        // So here's the idea: we just git git to dump out the graph. We divide
        // this graph buffer into chunks that begin at the start of a line that
        // contains a commit hash. This will look something like:
        /*

         | * |   e96277a570cd32432fjklfef
         | |\ \
         | | |/
         | |/|

        */
        // We want to display a) some more human-readable information about the
        // commit (i.e. what you get from logging with a more informative
        // --format) and b) our injected test status data. Overall this will
        // produce some other buffer. If it has less lines than the graph buffer
        // chunk, we can just append those lines onto the lines of the graph
        // buffer pairwise. If it has more lines then we will need to stretch
        // out the graph vertically to make space first.

        let graph_buf = repo
            .log_graph(range_spec.as_ref(), "%H\n")
            .await?
            // OsStr doesn't have a proper API, luckily we can expect utf-8.
            .into_string()
            .map_err(|_err| anyhow::anyhow!("got non-utf8 output from git log"))?;
        let graph_buf = graph_buf.trim();

        // Each chunk is a Vec of lines.
        let mut cur_chunk = Vec::<&str>::new();
        let mut chunks = Vec::<Vec<&str>>::new();
        for line in graph_buf.split('\n') {
            // --graph uses * to represent a node in the DAG.
            if line.contains('*') && !cur_chunk.is_empty() {
                chunks.push(mem::take(&mut cur_chunk));
            }
            if !line.is_empty() {
                cur_chunk.push(line);
            }
        }
        if !cur_chunk.is_empty() {
            chunks.push(cur_chunk);
        }

        let mut lines = Vec::new();
        let mut status_commits = HashMap::new();
        for mut chunk in chunks {
            // The commit hash should be the only alphanumeric sequence in
            // the chunk and it should be in the first line.
            let matches: Vec<_> = COMMIT_HASH_REGEX.find_iter(chunk[0]).collect();
            if matches.len() != 1 {
                bail!(
                    "matched {} commit hashes in graph chunk:\n{:?}",
                    matches.len(),
                    chunk
                );
            }
            let mattch = matches.first().unwrap();
            let hash = CommitHash::new(mattch.as_str());

            let log_n1_os = repo
                .log_n1(&hash, log_format)
                .await
                .context(format!("couldn't get commit data for {:?}", hash))?;
            // Hack: because OsStr doesn't have a proper API, luckily we can
            // just squash to utf-8, sorry users.
            let log_n1 = log_n1_os.to_string_lossy();

            // We're gonna add our own newlines in so we don't need the one that
            // Git printed.
            let log_n1 = log_n1.strip_suffix('\n').unwrap_or(&log_n1);

            // We only want the graph bit, strip out the commit hash which we
            // only put in there as an anchor for this algorithm.
            chunk[0] = &chunk[0][..mattch.range().start];

            let mut info_lines: Vec<&str> = log_n1.split('\n').collect();

            // Here's where we'll inject the live status
            status_commits.insert(lines.len() + info_lines.len(), hash);
            info_lines.push("");

            let graph_line_deficit = info_lines.len() as isize - chunk.len() as isize;
            let extension_line;
            if graph_line_deficit > 0 {
                // We assume that the first line of the chunk will contain an
                // asterisk identifying the current commit, and some vertical
                // lines continuing up to the previous chunk. We just copy those
                // vertical lines and then add a new vertical lines pointing up
                // to the asterisk.
                //
                // I checked and it is in fact possible to have non-vertical
                // lines on the same line as the asterisk. E.g. check the linux
                // kernel history, search back to commit 578cc98b66f5a5 and you
                // will see it. So we need to replace diagnoals with verticals
                // too.
                extension_line = GRAPH_COMPONENT_REGEX.replace_all(chunk[0], "|");
                for _ in 0..graph_line_deficit {
                    chunk.insert(1, &extension_line);
                }
            } else {
                // Append empty entries to the info lines so that the zip below works nicely.
                info_lines.append(&mut vec![""; -graph_line_deficit as usize]);
            }
            assert_eq!(info_lines.len(), chunk.len());

            lines.append(
                &mut chunk
                    .iter()
                    .zip(info_lines.iter())
                    .map(|(graph, info)| (*graph).to_owned() + *info)
                    // TODO: can we get rid of the collect and just call .join on the map iterator?
                    .collect::<Vec<_>>(),
            );
        }
        Ok(Self {
            lines,
            status_commits,
        })
    }

    fn render<'a>(
        &'a self,
        statuses: &'a HashMap<CommitHash, HashMap<TestName, TrackedTestCase>>,
        result_url_base: &str,
    ) -> anyhow::Result<Text<'a>> {
        if self.lines.is_empty() {
            return Ok("[range empty]".into());
        }
        self.lines
            .iter()
            .enumerate()
            .map(|(i, log_line)| -> anyhow::Result<Line> {
                let mut spans = vec![Span::from(log_line)];
                if let Some(hash) = self.status_commits.get(&i) {
                    if let Some(tracked_cases) = statuses.get(hash) {
                        spans.extend(self.render_cases(tracked_cases, result_url_base)?);
                    }
                }
                Ok(Line::from_iter(spans))
            })
            .collect::<anyhow::Result<Text>>()
    }

    fn render_cases<'a>(
        &self,
        tracked_cases: &'a HashMap<TestName, TrackedTestCase>,
        result_url_base: &str,
    ) -> anyhow::Result<Vec<Span<'a>>> {
        let mut tracked_cases: Vec<(&TestName, &TrackedTestCase)> = tracked_cases.iter().collect();
        // Sort by test case name. Would like sort_by_key here but
        // there's lifetime pain.
        tracked_cases.sort_by(|(name1, _), (name2, _)| name1.cmp(name2));
        let mut spans = Vec::new();
        for (name, tracked_case) in tracked_cases {
            let status_part = match &tracked_case.status {
                // Note - cancellation is an "error" in the type system but we
                // don't treat it as an error in the UI.
                TestStatus::Finished(Err(TestInconclusive::Error(msg))) => {
                    Span::new(msg).with_class(Class::Error)
                }
                TestStatus::Finished(Ok(result)) => {
                    if result.exit_code == 0 {
                        Span::new("success").with_class(Class::Success)
                    } else {
                        Span::new(format!("failed (status {})", result.exit_code))
                            .with_class(Class::Failure)
                    }
                }
                _ => Span::new(tracked_case.status.to_string()),
            }
            .with_url(format!(
                "{}/{}/stdout.txt",
                result_url_base,
                Database::result_relpath(&tracked_case.test_case).to_string_lossy()
            ));
            spans.extend([
                Span::new(name.to_string()).with_class(Class::TestName),
                Span::new(": "),
                status_part,
                Span::new(" "),
            ])
        }
        Ok(spans)
    }
}

#[cfg(test)]
mod tests {
    use core::str;
    use std::{sync::Arc, time::Duration};

    use googletest::{expect_that, prelude::eq};

    use crate::{
        git::{
            test_utils::{TempRepo, WorktreeExt},
            Commit,
        },
        test::{CachePolicy, Test, TestName, TestResult},
    };

    use super::*;

    fn fake_test(name: &str, cache_policy: CachePolicy) -> Arc<Test> {
        Arc::new(Test {
            name: TestName::new(name),
            cache_policy,
            // Don't care abou any of the other fields in these tests
            config_hash: 0,
            program: "".into(),
            args: vec![],
            needs_resources: [].into(),
            shutdown_grace_period: Duration::from_secs(1),
            depends_on: vec![],
        })
    }

    fn fake_notif(commit_hash: &CommitHash, test: &Arc<Test>, status: TestStatus) -> Notification {
        Notification {
            test_case: TestCase {
                commit_hash: commit_hash.clone(),
                cache_hash: Some(commit_hash.clone().into()),
                test: test.clone(),
            },
            status,
        }
    }

    // Abbreviate a commit message.
    fn abbrev(commit: &Commit) -> &str {
        // TODO: The degree of abbreviation here probably depends on git
        // configuration. This will probably fail on someone's machine.
        &<CommitHash as AsRef<str>>::as_ref(&commit.hash)[..7]
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_smoke() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        repo.commit("1").await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{}^..HEAD", commit2.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(
                &commit3.hash,
                &test2,
                TestStatus::Finished(Ok(TestResult { exit_code: 0 })),
            ),
            fake_notif(
                &commit2.hash,
                &test1,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").unwrap().ansi());
        expect_that!(
            // The colored crate does not have any useful way to disable it from
            // this test code, only globally. This clashes with parallel testing.
            // At first I thought about just having tests take a global lock but
            // then realied that if one test failed, the other would panic
            // holding the lock. Also parallelism is nice. So, we just ignore
            // the color.
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq(format!(
                "* {commit3} 3\n\
                | my_test1: Enqueued my_test2: success \n\
                * {commit2} 2\n\
                | my_test1: oh no my_test2: Started \n",
                commit3 = abbrev(&commit3),
                commit2 = abbrev(&commit2)
            ))
        );
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_octopus() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_commit = repo.commit("base").await.unwrap();
        let join_commit = repo.commit("join").await.unwrap();
        let commit1 = repo.commit("1").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        let merge = repo
            .merge(&[
                commit1.hash.clone(),
                commit2.hash.clone(),
                commit3.hash.clone(),
            ])
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{}..HEAD", base_commit.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");

        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(
                &commit3.hash,
                &test2,
                TestStatus::Finished(Ok(TestResult { exit_code: 0 })),
            ),
            fake_notif(
                &commit2.hash,
                &test1,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").unwrap().ansi());

        // Note this is a kinda weird log. We excluded the common ancestor of all the commits.
        // Also note it's a kinda weird input because we haven't provided any
        // statuses all of the commits (this does momentarily happen IRL).
        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq(format!(
                "*-.   {merge} merge commit\n\
                |\\ \\  \n\
                | | | \n\
                | | * {commit2} 2\n\
                | |   my_test1: oh no my_test2: Started \n\
                | * {commit1} 1\n\
                | | \n\
                | * {join} join\n\
                |   \n\
                * {commit3} 3\n\
                | my_test1: Enqueued my_test2: success \n",
                merge = abbrev(&merge),
                commit3 = abbrev(&commit3),
                commit2 = abbrev(&commit2),
                commit1 = abbrev(&commit1),
                join = abbrev(&join_commit)
            ))
        );
    }

    #[googletest::test]
    #[test_log::test(tokio::test)]
    async fn output_buffer_empty() {
        let repo = Arc::new(TempRepo::new().await.unwrap());
        let base_commit = repo.commit("base").await.unwrap();
        repo.commit("join").await.unwrap();
        let commit1 = repo.commit("1").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit2 = repo.commit("2").await.unwrap();
        repo.checkout(&base_commit.hash).await.unwrap();
        let commit3 = repo.commit("3").await.unwrap();
        repo.merge(&[commit1.hash, commit2.hash.clone(), commit3.hash.clone()])
            .await
            .unwrap();
        let test1 = fake_test("my_test1", CachePolicy::ByCommit);
        let test2 = fake_test("my_test2", CachePolicy::ByCommit);

        let ob = OutputBuffer::new(&repo, format!("{0}..{0}", base_commit.hash), "%h %s")
            .await
            .expect("failed to build OutputBuffer");
        let mut tracked_cases = HashMap::new();
        for notif in [
            fake_notif(&commit3.hash, &test1, TestStatus::Enqueued),
            fake_notif(
                &commit3.hash,
                &test1,
                TestStatus::Finished(Ok(TestResult { exit_code: 0 })),
            ),
            fake_notif(
                &commit2.hash,
                &test2,
                TestStatus::Finished(Err(TestInconclusive::Error("oh no".to_owned()))),
            ),
            fake_notif(&commit2.hash, &test2, TestStatus::Started),
        ] {
            update_tracked_cases(&mut tracked_cases, Arc::new(notif));
        }

        let buf = format!("{}", ob.render(&tracked_cases, "myhost").unwrap().ansi());
        expect_that!(
            *strip_ansi_escapes::strip_str(str::from_utf8(buf.as_bytes()).unwrap()),
            eq("[range empty]\n".to_owned())
        );
    }
}
