use anyhow::{anyhow, Context};
use clap::{arg, Parser as _, Subcommand};
use config::{Config, ParsedConfig};
use dag::{Dag, GraphNode as _};
use database::{Database, DatabaseEntry};
use futures::future::join_all;
use futures::StreamExt;
use git::{PersistentWorktree, TempWorktree};
use http::Ui;
use log::info;
use nix::sys::utsname::uname;
use resource::Pools;
use resource::{Resource, ResourceKey};
use std::borrow::Borrow as _;
use std::cmp::min;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{stdout, Stdout};
use std::path::PathBuf;
use std::pin::pin;
use std::process::Stdio;
use std::sync::Arc;
use std::{env, fs, str};
use tempfile::TempDir;
use test::TestResult;
use test::{
    base_job_env, Manager, TestCase, TestCaseId, TestJob, TestJobBuilder, TestJobOutput, TestName,
};
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;
use util::{DisplayablePathBuf, ErrGroup};

use crate::git::Worktree;

mod config;
mod dag;
mod database;
mod git;
mod http;
mod process;
mod resource;
mod status;
mod test;
mod util;

#[cfg(test)]
mod test_utils;

#[derive(clap::Parser)]
#[command(author, version, about, long_about = None)]
struct Args {
    // TODO: Don't require valid utf-8 strings here, OsStrings shoud be fine. But
    // https://stackoverflow.com/questions/76341332/clap-default-value-for-pathbuf
    #[arg(short, long, default_value_t = {".".to_string()})]
    repo: String,
    /// Path to TOML config file.
    #[arg(short, long, required = true)]
    config: PathBuf,
    /// Directory where results will be stored.
    #[arg(long, default_value_t = default_result_db())]
    result_db: DisplayablePathBuf,
    /// Filename prefix for temporary worktrees.
    #[arg(long, default_value_t = {"local-ci-worktree".to_string()})]
    worktree_prefix: String,
    /// Directory (must exist) to create temporary worktrees in.
    #[arg(long, default_value_t = {env::temp_dir().to_string_lossy().into_owned()})]
    worktree_dir: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct WatchArgs {
    /// Socket address in the form "$ip:$port" to listen on for serving files
    /// over HTTP. For example "127.0.0.1:8080" or ""[::1]:1234". Set the port
    /// to 0 to let the OS pick a port for us.
    #[arg(long, default_value_t = {"0.0.0.0:0".to_string()})]
    http_sockaddr: String,
    /// Hostname to use for HTTP URLs
    #[arg(long, default_value_t = default_hostname())]
    hostname: String,
    /// Base of range to test. Will test commits between this (exclusive) and
    /// HEAD (inclusive). Whenever HEAD changes, this string will be re-evaluated
    /// to find the base of the range.
    base: String,
}

fn default_result_db() -> DisplayablePathBuf {
    DisplayablePathBuf(
        directories::ProjectDirs::from("", "", "local-ci")
            .expect("couldn't find user data dir")
            .data_local_dir()
            .to_owned(),
    )
}

fn default_hostname() -> String {
    uname()
        .expect("couldn't get nodename")
        .nodename()
        .to_owned()
        .into_string()
        .expect("hostname wasn't utf-8")
}

#[derive(clap::Args)]
struct TestArgs {
    /// Name of the test to run, per the "name" field in the config file.
    test: String,
}

#[derive(Subcommand)]
enum Command {
    /// The main command. Watch a repository and run tests whenever the revision
    /// range changes.
    Watch(WatchArgs),
    /// Run a one-shot test in the specified repo. Do not cache the results.
    Test(TestArgs),
}

// Kitchen-sink object for global shit.
struct Env {
    config: ParsedConfig,
    repo: Arc<git::PersistentWorktree>,
    database: Database,
    worktree_builder: WorktreeBuilder,
}

// Fallback instead of https://github.com/Stebalien/tempfile/pull/308
struct WorktreeBuilder {
    prefix: OsString,
    parent_dir: PathBuf,
}

impl WorktreeBuilder {
    pub fn build(&self) -> anyhow::Result<TempDir> {
        tempfile::Builder::new()
            .prefix(&self.prefix)
            .tempdir_in(&self.parent_dir)
            .context("creating temp dir for worktree")
    }
}

// This is the main loop of the program. Take notifications from the Git tree,
// feed them to the test manager, feed the test manager's results to the status
// tracker (basically the UI).
async fn watch_loop(
    cancellation_token: CancellationToken,
    test_manager: Arc<test::Manager<PersistentWorktree>>,
    mut status_tracker: status::Tracker<PersistentWorktree, Stdout>,
    range_spec: OsString,
    repo: Arc<PersistentWorktree>,
) -> anyhow::Result<()> {
    let mut revs_stream = pin!(repo.watch_refs(&range_spec)?);
    let mut notifs = test_manager.results();
    loop {
        select! {
            // TODO: It's dumb that we have two different types of communication here (one exposes
            // the channel, one implements Stream).
            revs = revs_stream.next() => {
                // TODO: figure out if/how this can actually fail.
                let revs = revs.expect("revset stream terminated")?;
                // Paying for a pointless clone here so we can do set_revisions
                // (mostly just kicks off background stuff) before awaiting the
                // status tracker reset (does synchronhous work).
                test_manager.set_revisions(revs.clone()).await.context("setting revisions to test")?;
                status_tracker.set_range(&range_spec).await.context("resetting status tracker")?;
                status_tracker.repaint().context("error painting status to stdout")?;
            },
            notif = notifs.recv() => {
                // https://github.com/rust-lang/futures-rs/issues/1857
                // AFAICS there is no way to encode a stream that never terminates.
                let notif = notif.expect("notification stream terminated");
                status_tracker.update(notif);
                status_tracker.repaint().context("error painting status to stdout")?;
            },
            _ =  cancellation_token.cancelled() => {
                info!("Got shutdown signal, terminating jobs and waiting");
                test_manager.cancel_running().await.context("cancelling tests")?;
                // Ensure jobs are shut down before we delort stuff etc.
                test_manager.settled().await;
                break;
            }
        }
    }
    Ok(())
}

async fn watch(
    env: Env,
    cancellation_token: CancellationToken,
    watch_args: WatchArgs,
) -> anyhow::Result<()> {
    // Create HTTP server, to serve the result artifacts to the user when they
    // click terminal hyperlinks.
    let listener = tokio::net::TcpListener::bind(watch_args.http_sockaddr.clone())
        .await
        .unwrap();
    let ui = Ui::new(
        watch_args.hostname.clone(),
        listener,
        env.database.base_dir.clone(),
    );
    let result_url_base = ui.result_url_base()?;
    let home_url = ui.home_url()?;
    tokio::spawn(ui.serve());

    let Env {
        repo,
        database,
        config:
            ParsedConfig {
                num_worktrees,
                resource_pools,
                tests,
            },
        worktree_builder,
    } = env;
    let resource_pools = Arc::new(resource_pools);

    // Set up the test manager, which is the weirdly-scoped god-object that
    // orchestrates test jobs.
    let test_manager = Arc::new(Manager::new(
        repo.clone(),
        database,
        resource_pools.clone(),
        tests,
    ));

    // Set up the status tracker, which shows the user what's going on in the terminal.
    let status_tracker = status::Tracker::new(repo.clone(), stdout(), result_url_base, home_url);

    // Kick off creation of the worktrees that the test manager will run jobs in.
    //
    // Once we've done this, we can no longer return from this function until
    // we've also cleaned the worktrees up. This is stinky and gross. AFAICT
    // async Rust just doesn't have a solution for that at all.
    //
    // TODO: This doesn't work if there are no commits in the repository. Not sure I care about
    // this, but the solution would be to create the worktrees ondemand, when we have a revision we
    // are actually trying to test. That might be a good idea anyway, so probably it's preferable to
    // just do that for its own sake and leave the empty-repo problem as a nice freebie.
    let mut eg = ErrGroup::new(cancellation_token.clone());
    for _ in 0..num_worktrees {
        let repo = repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = resource_pools.clone();
        let dir = worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    // DO THE THING.
    eg.spawn(watch_loop(
        cancellation_token.child_token(),
        test_manager.clone(),
        status_tracker,
        format!("{}..HEAD", watch_args.base).into(),
        repo,
    ));

    let end_result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    join_all(
        Arc::into_inner(test_manager)
            .expect("leaked test manager reference")
            .into_resource_pools()
            .try_remove_worktrees()
            .map(|w| w.cleanup()),
    )
    .await;

    end_result
}

async fn ensure_job_success(
    resource_pools: Arc<Pools>,
    mut job: TestJob<DatabaseEntry>,
    origin_worktree: PathBuf,
) -> anyhow::Result<()> {
    job.await_dep_success()
        .await
        .map_err(|name| anyhow!("dependency job {name:?} failed"))?;
    job.get_resources_and_run(resource_pools.as_ref(), &origin_worktree)
        .await?;
    Ok(())
}

struct OneshotOutput {}

impl TestJobOutput for OneshotOutput {
    fn stdout(&mut self) -> anyhow::Result<Stdio> {
        Ok(Stdio::inherit())
    }
    fn stderr(&mut self) -> anyhow::Result<Stdio> {
        Ok(Stdio::inherit())
    }
    fn set_result(&mut self, result: &TestResult) -> anyhow::Result<()> {
        println!("Job result: {result:?}");
        Ok(())
    }
}

async fn test(
    env: Env,
    cancellation_token: CancellationToken,
    test_args: &TestArgs,
) -> anyhow::Result<()> {
    let test_name = TestName::new(test_args.test.clone());
    // So we can cache the results in the database, the dependency jobs will be run at HEAD.
    let head = env
        .repo
        .rev_parse("HEAD")
        .await
        .context("failed to look up HEAD commit")?
        .ok_or(anyhow!("no HEAD commit - repo empty?"))?;
    let job_env = Arc::new(base_job_env(env.repo.path()));

    let Env {
        repo,
        database,
        config:
            ParsedConfig {
                num_worktrees,
                resource_pools,
                tests,
            },
        worktree_builder,
    } = env;

    // Only need worktrees for the tests that needs worktrees.
    let dep_tests = tests
        .top_down_from(&test_name)
        .ok_or(anyhow!("no such test {:?}", test_name.to_string()))?
        // Exclude the main test, we'll run that separately.
        .skip(1);
    let num_worktrees = min(
        num_worktrees,
        dep_tests.clone().filter(|t| t.needs_worktree()).count(),
    );

    // Get the graph of tests we need to run as dependencies.
    // This is kinda inefficient: we're building a new Dag based on a subset of
    // the old one, so the validation in the constructor is not strictly
    // necessary. There are two levels of optimisation we could do here:
    // 1. We could add a way to build the new Dag from the TopDown iterator
    //    or some derivative of it, and use the type safety to just skip
    //    validation
    // 2. We could build the subset graph in place, i.e. totally skip making a
    //    new graph and instead just logicall remove the nodes we don't need.
    let dep_jobs = Dag::new(
        // TODO: Would be nice to have an _into thing so we can avoid this clone.
        dep_tests.map(|t| TestCase::new(head.clone(), t.clone())),
    )
    .context("setting up dependency test graph")?
    .bottom_up()
    .try_fold(
        HashMap::new(),
        |mut jobs, test_case| -> anyhow::Result<HashMap<TestCaseId, TestJob<DatabaseEntry>>> {
            let wait_for = test_case
                .child_ids() // This gives the TestCaseIds of dependency jobs.
                .iter()
                .map(|tc_id| {
                    (
                        test_case.test.name.clone(),
                        jobs[tc_id.borrow()].subscribe_completion(),
                    )
                })
                .collect();
            let job = TestJobBuilder::new(
                cancellation_token.clone(),
                // TODO: it would be nice if we had an into_ variant of
                // the bottom_up so we didn't need this clone.
                test_case.clone(),
                database.create_output(test_case)?,
                job_env.clone(),
                wait_for,
            )
            .build();
            jobs.insert(test_case.id().borrow().to_owned(), job);
            Ok(jobs)
        },
    )?;

    println!("Running {} dependency jobs", dep_jobs.len());

    // Kick off creation of the worktrees that the dep jobs will run in.
    // This is horribly copy-pasted from watch. I dunno, I can't figure out how
    // to fix that without insanely complex async stuff.
    let resource_pools = Arc::new(resource_pools);
    let mut eg = ErrGroup::new(cancellation_token.clone());
    for _ in 0..num_worktrees {
        let repo = repo.clone();
        let ct = cancellation_token.child_token();
        let resource_pools = resource_pools.clone();
        let dir = worktree_builder.build()?;
        eg.spawn(async move {
            let worktree = TempWorktree::new::<PersistentWorktree>(&ct, repo.as_ref(), dir).await?;
            resource_pools.add([(ResourceKey::Worktree, Resource::Worktree(worktree))]);
            Ok(())
        });
    }

    for (_, job) in dep_jobs {
        eg.spawn(ensure_job_success(
            resource_pools.clone(),
            job,
            repo.path.clone(),
        ))
    }

    let end_result = eg.wait().await;

    // Now we have to remember to clean up before returning the result :/
    join_all(resource_pools.try_remove_worktrees().map(|w| w.cleanup())).await;

    end_result?;
    println!("Dependency jobs complete");

    let test = tests.node(&test_name).unwrap();
    let test_case = TestCase::new(head.clone(), test.clone());
    let mut needs_resources = test_case.test.needs_resources.clone();
    let mut job = TestJobBuilder::new(
        cancellation_token.clone(),
        test_case,
        OneshotOutput {},
        job_env.clone(),
        Vec::new(), // wait_for
    )
    .build();
    // Doesn't need a worktree, it's gonna do it live and direct in the main tree.
    needs_resources.remove(&ResourceKey::Worktree);
    let resources = resource_pools.get(needs_resources).await;
    match job.run(repo.path(), &resources).await? {
        None => println!("Canceled"),
        Some(exit_code) => println!("Job completed with exit code {exit_code}"),
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    // Set up shutdown first, to ensure we correctly handle early signals.
    // As well as doing it early, it seems to be important that we do this with
    // a single global signal::ctrl_c call, if I call this in the select loop I
    // would occasionally observe that SIGINT kills the program instead of
    // triggering Tokio's signal handler, this is because we require the ctrl_c
    // future to get polled before we do any work, since that's where it
    // installs the signal handler.
    // TOOD: this still seems racy though, because in theory we could get all
    // the way into the setup below before the task here ever gets to polling
    // the future. It seems like this just means the ctrl_c design is bad and we
    // should probably just not use it.
    let cancellation_token = CancellationToken::new();
    let token = cancellation_token.clone();
    tokio::spawn(async move {
        signal::ctrl_c().await.expect("error listening for ctrl-C");
        token.cancel()
    });

    let args = Args::parse();
    let config_content = fs::read_to_string(&args.config).context("couldn't read config")?;
    let config: Config = toml::from_str(&config_content).context("couldn't parse config")?;
    let config = ParsedConfig::from(config)?;

    let repo = git::PersistentWorktree {
        path: args.repo.to_owned().into(),
    };
    // Check repo is valid.
    repo.git_common_dir()
        .await
        .context(format!("opening repo {}", args.repo))?;

    let env = Env {
        config,
        repo: Arc::new(repo),
        database: Database::create_or_open(&args.result_db)?,
        worktree_builder: WorktreeBuilder {
            prefix: args.worktree_prefix.into(),
            parent_dir: args.worktree_dir.into(),
        },
    };

    match args.command {
        Command::Watch(watch_args) => watch(env, cancellation_token, watch_args).await,
        Command::Test(ref test_args) => test(env, cancellation_token, test_args).await,
    }
}
