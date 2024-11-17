use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};

use anyhow::Context as _;
use axum::{
    extract::State,
    handler::HandlerWithoutStateExt as _,
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use indoc::indoc;
use tokio::{net::TcpListener, select};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

async fn handle_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "File not found")
}

pub struct Ui {
    hostname: String,
    listener: TcpListener,
    // Path where we'll serve results from the result database. The results are just
    // served as a directory.
    result_db: PathBuf,
    state: Arc<UiState>,
}

impl Ui {
    pub fn new(hostname: String, listener: TcpListener, result_db: PathBuf) -> Self {
        Self {
            hostname,
            listener,
            result_db,
            state: Arc::new(UiState {
                log_buf: RwLock::new("[starting up...]".into()),
            }),
        }
    }

    pub fn home_url(&self) -> anyhow::Result<String> {
        Ok(format!(
            "http://{}:{}",
            self.hostname,
            self.listener
                .local_addr()
                .context("getting local socket addr")?
                .port()
        ))
    }

    pub fn result_url_base(&self) -> anyhow::Result<String> {
        Ok(self.home_url()? + "/results")
    }

    pub fn state(&self) -> Arc<UiState> {
        self.state.clone()
    }

    pub async fn serve(self, ct: CancellationToken) -> anyhow::Result<()> {
        let app = Router::new()
            .route("/", get(home))
            .nest_service(
                "/results",
                ServeDir::new(self.result_db).not_found_service(handle_404.into_service()),
            )
            .with_state(self.state);
        select! {
            result = axum::serve(self.listener, app) => result.context("serving web UI"),
            _ = ct.cancelled() => Ok(()),
        }
    }
}

pub struct UiState {
    // This holds the pre-rendered log & test result buffer with links etc.
    log_buf: RwLock<String>,
}

impl UiState {
    pub fn set_log_buf(&self, buf: String) {
        *self.log_buf.write().unwrap() = buf;
    }
}

async fn home(State(state): State<Arc<UiState>>) -> Html<String> {
    format!(
        indoc! {r#"
        <!DOCTYPE html>
            <html lang="en">
            <head>
                <meta charset="utf-8">
                <title>title</title>
            </head>
            <body>
                <p>{}</p>
            </body>
        </html>
    "#},
        *state.log_buf.read().unwrap()
    )
    .into()
}
