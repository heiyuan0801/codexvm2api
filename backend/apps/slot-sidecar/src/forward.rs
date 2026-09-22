//! 经过固定代理的受限 OpenAI 上游转发。

use std::collections::BTreeMap;

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Response, StatusCode};
use reqwest::Client;
use serde::Deserialize;
use url::Url;

const ALLOWED_PATHS: &[&str] = &["/backend-api/codex/responses"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForwardRequest {
    pub path: String,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    pub body: serde_json::Value,
}

pub async fn forward(
    client: &Client,
    upstream_origin: &Url,
    request: ForwardRequest,
) -> Result<Response<Body>, ForwardError> {
    let target = upstream_url(upstream_origin, &request.path)?;
    let mut builder = client.post(target).json(&request.body);
    for (name, value) in request.headers {
        let name = HeaderName::try_from(name).map_err(|_| ForwardError::InvalidHeader)?;
        if filtered_request_header(&name) {
            continue;
        }
        let value = HeaderValue::try_from(value).map_err(|_| ForwardError::InvalidHeader)?;
        builder = builder.header(name, value);
    }
    let upstream = builder.send().await.map_err(|_| ForwardError::Upstream)?;
    let status = upstream.status();
    let headers = upstream.headers().clone();
    let mut response = Response::builder().status(status);
    copy_response_headers(&headers, response.headers_mut().expect("response headers"));
    response
        .body(Body::from_stream(upstream.bytes_stream()))
        .map_err(|_| ForwardError::Upstream)
}

fn upstream_url(origin: &Url, path: &str) -> Result<Url, ForwardError> {
    let parsed = Url::parse(&format!("https://slot.invalid{path}"))
        .map_err(|_| ForwardError::InvalidPath)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host_str() != Some("slot.invalid")
        || !ALLOWED_PATHS.contains(&parsed.path())
    {
        return Err(ForwardError::InvalidPath);
    }
    let mut target = origin.clone();
    target.set_path(parsed.path());
    target.set_query(parsed.query());
    Ok(target)
}

fn filtered_request_header(name: &HeaderName) -> bool {
    matches!(
        name.as_str(),
        "connection"
            | "content-length"
            | "host"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    ) || name.as_str().starts_with("x-cpr-slot-")
}

fn copy_response_headers(source: &HeaderMap, target: &mut HeaderMap) {
    for (name, value) in source {
        if !matches!(
            name.as_str(),
            "connection" | "content-length" | "transfer-encoding" | "upgrade"
        ) {
            target.append(name, value.clone());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ForwardError {
    #[error("slot forward path is invalid")]
    InvalidPath,
    #[error("slot forward header is invalid")]
    InvalidHeader,
    #[error("slot upstream request failed")]
    Upstream,
}

impl ForwardError {
    pub const fn status(self) -> StatusCode {
        match self {
            Self::InvalidPath | Self::InvalidHeader => StatusCode::BAD_REQUEST,
            Self::Upstream => StatusCode::BAD_GATEWAY,
        }
    }
}
