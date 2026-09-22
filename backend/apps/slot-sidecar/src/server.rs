//! sidecar HTTP 边界与认证。

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, Response, StatusCode, header};
use axum::routing::{get, post};
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::config::SidecarConfig;
use crate::forward::{ForwardRequest, forward};

struct SidecarState {
    token: Vec<u8>,
    client: reqwest::Client,
    upstream_origin: url::Url,
}

pub async fn build_router(config: &SidecarConfig) -> Result<Router, SidecarBuildError> {
    let token = read_file(&config.auth_file, SidecarBuildError::AuthFile).await?;
    if token.len() < 32 || token.len() > 4096 {
        return Err(SidecarBuildError::AuthFile);
    }
    let proxy = read_file(&config.proxy_file, SidecarBuildError::ProxyFile).await?;
    let proxy = std::str::from_utf8(&proxy).map_err(|_| SidecarBuildError::ProxyFile)?;
    let proxy = reqwest::Proxy::all(proxy.trim()).map_err(|_| SidecarBuildError::InvalidProxy)?;
    let client = reqwest::Client::builder()
        .proxy(proxy)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| SidecarBuildError::Client)?;
    let state = Arc::new(SidecarState {
        token,
        client,
        upstream_origin: config.upstream_origin.clone(),
    });
    Ok(Router::new()
        .route("/readyz", get(ready))
        .route("/internal/v1/forward", post(handle_forward))
        .with_state(state))
}

pub async fn serve(config: SidecarConfig) -> Result<(), SidecarBuildError> {
    let router = build_router(&config).await?;
    let listener = TcpListener::bind(config.listen)
        .await
        .map_err(|_| SidecarBuildError::Listen)?;
    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|_| SidecarBuildError::Serve)
}

async fn ready() -> StatusCode {
    StatusCode::OK
}

async fn handle_forward(
    State(state): State<Arc<SidecarState>>,
    headers: HeaderMap,
    Json(request): Json<ForwardRequest>,
) -> Response<Body> {
    if !authorized(&headers, &state.token) {
        return sanitized_error(StatusCode::UNAUTHORIZED, "unauthorized");
    }
    match forward(&state.client, &state.upstream_origin, request).await {
        Ok(response) => response,
        Err(error) => sanitized_error(error.status(), error.to_string()),
    }
}

fn authorized(headers: &HeaderMap, expected: &[u8]) -> bool {
    let Some(value) = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
    else {
        return false;
    };
    value.len() == expected.len() && bool::from(value.ct_eq(expected))
}

fn sanitized_error(status: StatusCode, message: impl Into<String>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            serde_json::json!({ "error": { "message": message.into() } }).to_string(),
        ))
        .expect("static error response")
}

async fn read_file(
    path: &std::path::Path,
    error: SidecarBuildError,
) -> Result<Vec<u8>, SidecarBuildError> {
    let value = tokio::fs::read(path).await.map_err(|_| error)?;
    let value = value
        .strip_suffix(b"\r\n")
        .or_else(|| value.strip_suffix(b"\n"))
        .unwrap_or(&value)
        .to_vec();
    if value.is_empty() || value.len() > 4096 {
        return Err(error);
    }
    Ok(value)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SidecarBuildError {
    #[error("slot authentication file is invalid")]
    AuthFile,
    #[error("slot proxy file is invalid")]
    ProxyFile,
    #[error("slot proxy URL is invalid")]
    InvalidProxy,
    #[error("slot HTTP client could not initialize")]
    Client,
    #[error("slot listener could not bind")]
    Listen,
    #[error("slot HTTP server failed")]
    Serve,
}
