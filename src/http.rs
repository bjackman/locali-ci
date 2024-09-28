use std::path::PathBuf;

use axum::{
    handler::HandlerWithoutStateExt as _, http::StatusCode, response::IntoResponse, Router,
};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

async fn handle_404() -> impl IntoResponse {
    (StatusCode::NOT_FOUND, "File not found")
}

pub async fn serve_dir(listener: TcpListener, path: PathBuf) {
    let app = Router::new().nest_service(
        "/",
        ServeDir::new(path).not_found_service(handle_404.into_service()),
    );
    axum::serve(listener, app).await.unwrap();
}
