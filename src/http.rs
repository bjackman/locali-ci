use std::path::PathBuf;

use anyhow::Context as _;
use axum::{
    handler::HandlerWithoutStateExt as _, http::StatusCode, response::IntoResponse, Router,
};
use tokio::net::TcpListener;
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
}

impl Ui {
    pub fn new(hostname: String, listener: TcpListener, result_db: PathBuf) -> Self {
        Self {
            hostname,
            listener,
            result_db,
        }
    }

    pub fn result_url_base(&self) -> anyhow::Result<String> {
        Ok(format!(
            "http://{}:{}",
            self.hostname,
            self.listener
                .local_addr()
                .context("getting local socket addr")?
                .port()
        ))
    }

    pub async fn serve(self) {
        let app = Router::new().nest_service(
            "/",
            ServeDir::new(self.result_db).not_found_service(handle_404.into_service()),
        );
        axum::serve(self.listener, app).await.unwrap();
    }
}
