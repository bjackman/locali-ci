use anyhow::Context;
use clap::{arg, Parser as _, Subcommand};
use config::Config;
use futures::StreamExt;
use log::info;
use nix::sys::utsname::uname;
use result::Database;
use std::ffi::OsString;
use std::io::stdout;
use std::path::PathBuf;
use std::pin::pin;
use std::sync::Arc;
use std::{env, fs, str};
use test::Manager;
use tokio::{select, signal};
use tokio_util::sync::CancellationToken;
use util::DisplayablePathBuf;

use crate::git::Worktree;

mod config;
mod dag;
mod git;
mod http;
mod process;
mod resource;
mod result;
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
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Args)]
struct WatchArgs {
    /// Filename prefix for temporary worktrees.
    #[arg(long, default_value_t = {"local-ci-worktree".to_string()})]
    worktree_prefix: String,
    /// Directory (must exist) to create temporary worktrees in.
    #[arg(long, default_value_t = {env::temp_dir().to_string_lossy().into_owned()})]
    worktree_dir: String,
    /// Socket address in the form "$ip:$port" to listen on for serving files
    /// over HTTP. For example "127.0.0.1:8080" or ""[::1]:1234". Set the port
    /// to 0 to let the OS pick a port for us.
    #[arg(long, default_value_t = {"0.0.0.0:0".to_string()})]
    http_sockaddr: String,
    /// Directory where results will be stored.
    #[arg(long, default_value_t = default_result_db())]
    result_db: DisplayablePathBuf,
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
    config: Config,
    repo: Arc<git::PersistentWorktree>,
}

async fn watch(
    env: Env,
    cancellation_token: CancellationToken,
    watch_args: &WatchArgs,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(watch_args.http_sockaddr.clone())
        .await
        .unwrap();
    let http_port = listener
        .local_addr()
        .expect("couldn't get local HTTP address")
        .port();
    tokio::spawn(http::serve_dir(listener, (*watch_args.result_db).clone()));

    let resource_tokens = env.config.parse_resource_tokens();
    let manager_builder = Manager::builder(
        env.repo.clone(),
        Database::create_or_open(&watch_args.result_db)?,
        env.config.parse_tests(&resource_tokens)?,
        resource_tokens,
    )
    .num_worktrees(env.config.num_worktrees)
    .worktree_prefix(&watch_args.worktree_prefix)
    .worktree_dir(&watch_args.worktree_dir);
    let mut test_manager = manager_builder.build().await?;
    let range_spec: OsString = format!("{}..HEAD", watch_args.base).into();
    let mut notifs = test_manager.results();
    let result_url_base = format!("http://{}:{}", watch_args.hostname, http_port);
    let mut status_tracker = status::Tracker::new(env.repo.clone(), stdout(), result_url_base);
    let mut revs_stream = env.repo.watch_refs(&range_spec)?;
    let mut revs_stream = pin!(revs_stream);
    loop {
        select!(
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
                test_manager.set_revisions([]).await.context("cancelling tests")?;
                break;
            }
        )
    }
    test_manager.settled().await;
    Ok(())
}

async fn test(
    _env: Env,
    _cancellation_token: CancellationToken,
    _test_args: &TestArgs,
) -> anyhow::Result<()> {
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
    };

    match args.command {
        Command::Watch(ref watch_args) => watch(env, cancellation_token, watch_args).await,
        Command::Test(ref test_args) => test(env, cancellation_token, test_args).await,
    }
}
