use std::{path::PathBuf, sync::Arc};

use anyhow::Context as _;
use axum::{
    extract::{
        ws::{Message, WebSocket},
        State, WebSocketUpgrade,
    },
    handler::HandlerWithoutStateExt as _,
    http::StatusCode,
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use indoc::indoc;
use tokio::{net::TcpListener, select, sync::watch};
use tokio_util::sync::CancellationToken;
use tower_http::services::ServeDir;

use crate::text::RenderHtmlPre;

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
    pub fn new(hostname: String, listener: TcpListener, result_db: PathBuf, title: String) -> Self {
        Self {
            hostname,
            listener,
            result_db,
            state: Arc::new(UiState::new(title)),
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
            .route("/updates", get(updates))
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
    log_html_pre: watch::Sender<String>,
    title: String,
}

impl UiState {
    fn new(title: String) -> Self {
        Self {
            log_html_pre: watch::Sender::new("[starting up...]".into()),
            title,
        }
    }

    pub fn set_log_buf(&self, render: RenderHtmlPre) {
        self.log_html_pre.send_replace(render.to_string());
    }
}

// Handles request to create a websocket.
async fn updates(ws: WebSocketUpgrade, State(state): State<Arc<UiState>>) -> Response {
    ws.on_upgrade(|socket| handle_socket(socket, state))
}

// This is the state machine for the websocket. It dumps out entire log buffers
// as <pre> with the id "log_buf".
async fn handle_socket(mut socket: WebSocket, state: Arc<UiState>) {
    // TODO: Example code - just send updates every second.
    let mut rx = state.log_html_pre.subscribe();
    loop {
        // Note the first update here will commonly be redundant, we just do it
        // because we don't know if log_buf changed since the client got the one
        // in the initial GET response.
        let buf = format!(
            r#"<pre id="log_buf">{}</pre>"#,
            rx.borrow_and_update().clone()
        );
        if socket.send(Message::Text(buf)).await.is_err() {
            // Client disconnected.
            return;
        }
        // Error should be impossible here; that would mean the sender got
        // dropped, but _we_ have a reference to the sender here (in the UiState).
        rx.changed().await.unwrap();
    }
}

async fn home(State(state): State<Arc<UiState>>) -> Html<String> {
    // This HTMX starts up with the current log buffer in a <pre> and connects
    // to the updates socket. The updates socket has the correct ID to replace
    // the <pre> every time there's an update.
    format!(
        indoc! {r#"
        <!DOCTYPE html>
            <html lang="en">
            <head>
                <meta charset="utf-8">
                <script>{htmx_js}</script>
                <script>{htmx_wx_js}</script>
                <style>{css}</style>
                <title>{title}</title>
            </head>
            <body>
                <div hx-ext="ws" ws-connect="/updates">
                    <pre id="log_buf">{log_buf}</pre>
                </div>
            </body>
        </html>
    "#},
        htmx_js = include_str!("htmx-2.0.3.min.js"),
        htmx_wx_js = include_str!("htmx-wx-ext-2.0.1.js"),
        log_buf = *state.log_html_pre.borrow(),
        css = RenderHtmlPre::CSS,
        title = state.title,
    )
    .into()
}
