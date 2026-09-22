use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::thread;

use axum::body::{Body, to_bytes};
use codex_slot_sidecar::{SidecarConfig, build_router};
use http::{Request, StatusCode, header};
use tempfile::TempDir;
use tower::ServiceExt;
use url::Url;

const TOKEN: &str = "slot-test-token-with-at-least-32-bytes";

#[tokio::test]
async fn forward_requires_the_slot_bearer_token() {
    let fixture = Fixture::new("http://127.0.0.1:9");
    let router = build_router(&fixture.config()).await.expect("router");
    let response = router
        .oneshot(forward_request(None, "/backend-api/codex/responses"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn forward_rejects_paths_outside_the_codex_responses_endpoint() {
    let fixture = Fixture::new("http://127.0.0.1:9");
    let router = build_router(&fixture.config()).await.expect("router");
    let response = router
        .oneshot(forward_request(Some(TOKEN), "/admin/secrets"))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn forward_uses_the_required_proxy_and_streams_the_response() {
    let proxy = CaptureProxy::start();
    let fixture = Fixture::new(&format!("http://{}", proxy.address));
    let router = build_router(&fixture.config()).await.expect("router");
    let response = router
        .oneshot(forward_request(
            Some(TOKEN),
            "/backend-api/codex/responses?stream=true",
        ))
        .await
        .expect("response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["x-upstream-test"], "proxied");
    let body = to_bytes(response.into_body(), 1024)
        .await
        .expect("streamed body");
    assert_eq!(body, "data: proxied\n\n");
    let captured = proxy.join();
    assert!(
        captured
            .starts_with("POST http://upstream.invalid/backend-api/codex/responses?stream=true")
    );
    assert!(
        captured
            .to_ascii_lowercase()
            .contains("authorization: bearer upstream-token")
    );
    assert!(!captured.to_ascii_lowercase().contains("x-cpr-slot-secret"));
}

fn forward_request(token: Option<&str>, path: &str) -> Request<Body> {
    let body = serde_json::json!({
        "path": path,
        "headers": {
            "authorization": "Bearer upstream-token",
            "content-type": "application/json",
            "x-cpr-slot-secret": "must-not-leak"
        },
        "body": { "model": "gpt-test", "stream": true }
    });
    let mut request = Request::builder()
        .method("POST")
        .uri("/internal/v1/forward")
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
    }
    request.body(Body::from(body.to_string())).expect("request")
}

struct Fixture {
    directory: TempDir,
    proxy_url: String,
}

impl Fixture {
    fn new(proxy_url: &str) -> Self {
        Self {
            directory: tempfile::tempdir().expect("fixture directory"),
            proxy_url: proxy_url.to_owned(),
        }
    }

    fn config(&self) -> SidecarConfig {
        let auth_file = self.write("auth", TOKEN);
        let proxy_file = self.write("proxy", &self.proxy_url);
        SidecarConfig::new(
            "127.0.0.1:0".parse().expect("listen"),
            auth_file,
            proxy_file,
            Url::parse("http://upstream.invalid").expect("origin"),
        )
    }

    fn write(&self, name: &str, value: &str) -> PathBuf {
        let path = self.directory.path().join(name);
        std::fs::write(&path, value).expect("fixture secret");
        path
    }
}

struct CaptureProxy {
    address: SocketAddr,
    task: thread::JoinHandle<String>,
}

impl CaptureProxy {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("proxy listener");
        let address = listener.local_addr().expect("proxy address");
        let task = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("proxy connection");
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("read timeout");
            let mut buffer = vec![0_u8; 8192];
            let size = stream.read(&mut buffer).expect("proxy request");
            let request = String::from_utf8_lossy(&buffer[..size]).into_owned();
            let response = concat!(
                "HTTP/1.1 200 OK\r\n",
                "content-type: text/event-stream\r\n",
                "x-upstream-test: proxied\r\n",
                "content-length: 15\r\n",
                "\r\n",
                "data: proxied\n\n"
            );
            stream
                .write_all(response.as_bytes())
                .expect("proxy response");
            request
        });
        Self { address, task }
    }

    fn join(self) -> String {
        self.task.join().expect("proxy task")
    }
}
