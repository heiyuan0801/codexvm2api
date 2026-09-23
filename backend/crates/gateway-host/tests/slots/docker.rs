use std::io::{Cursor, Read};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::to_bytes;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use gateway_core::account::{AccountSlotInstanceId, OutboundProxy};
use gateway_host::config::OpenAiSlotsConfig;
use gateway_host::slots::{
    AccountSlotEngine, AccountSlotEngineErrorKind, BollardAccountSlotEngine,
    egress::{EgressError, SlotEgressLifecycle},
};
use serde_json::{Value, json};

const SUFFIX: &str = "0199f4c852a87aa0a6d7f75219e82e3d";

#[derive(Default)]
struct DockerCapture {
    requests: Vec<(Method, String, Vec<u8>)>,
    foreign_volume: bool,
    network_missing: bool,
    events: Arc<Mutex<Vec<String>>>,
}

struct DockerFixture {
    engine: BollardAccountSlotEngine,
    capture: Arc<Mutex<DockerCapture>>,
    events: Arc<Mutex<Vec<String>>>,
    server: tokio::task::JoinHandle<()>,
    _directory: tempfile::TempDir,
}

impl DockerFixture {
    async fn start(foreign_volume: bool) -> Self {
        Self::start_with_options(foreign_volume, false).await
    }

    async fn start_with_network_missing(foreign_volume: bool) -> Self {
        Self::start_with_options(foreign_volume, true).await
    }

    async fn start_with_options(foreign_volume: bool, network_missing: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("docker.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let capture = Arc::new(Mutex::new(DockerCapture {
            foreign_volume,
            network_missing,
            events: Arc::new(Mutex::new(Vec::new())),
            ..DockerCapture::default()
        }));
        let events = capture.lock().unwrap().events.clone();
        let router = axum::Router::new()
            .fallback(docker_request)
            .with_state(capture.clone());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let engine = BollardAccountSlotEngine::connect_with_egress(
            OpenAiSlotsConfig {
                enabled: true,
                docker_endpoint: format!("unix://{}", socket.display()),
                ..OpenAiSlotsConfig::default()
            },
            "0123456789ab",
            Arc::new(FakeEgress {
                events: Arc::clone(&events),
            }),
        )
        .unwrap();
        Self {
            engine,
            capture,
            events,
            server,
            _directory: directory,
        }
    }
}

impl Drop for DockerFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn docker_request(
    State(state): State<Arc<Mutex<DockerCapture>>>,
    request: Request,
) -> Response {
    let method = request.method().clone();
    let uri = request.uri().to_string();
    let body = to_bytes(request.into_body(), 1024 * 1024).await.unwrap();
    let (foreign_volume, network_missing) = {
        let mut capture = state.lock().unwrap();
        capture
            .requests
            .push((method.clone(), uri.clone(), body.to_vec()));
        capture
            .events
            .lock()
            .unwrap()
            .push(format!("docker {method} {uri}"));
        let network_missing = capture.network_missing
            && method == Method::GET
            && uri.contains("/networks/")
            && !uri.contains("/networks/create");
        if network_missing {
            capture.network_missing = false;
        }
        (capture.foreign_volume, network_missing)
    };
    let labels = json!({
        "io.codex-proxy-rs.owner": "openai-account-slot",
        "io.codex-proxy-rs.slot.instance": "slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d"
    });
    let (status, body) = if uri.contains("/volumes/") {
        let labels = if foreign_volume {
            json!({"io.codex-proxy-rs.owner": "some-other-app"})
        } else {
            labels
        };
        (
            StatusCode::OK,
            json!({"Name": format!("cpr-slot-{SUFFIX}-data"), "Driver": "local", "Mountpoint": "/data", "Labels": labels, "Scope": "local", "Options": {}}),
        )
    } else if method == Method::POST && uri.contains("/networks/create") {
        (
            StatusCode::CREATED,
            json!({"Id": "fake-network", "Warning": ""}),
        )
    } else if uri.contains("/networks/") && network_missing {
        (
            StatusCode::NOT_FOUND,
            json!({"message": "network not found"}),
        )
    } else if uri.contains("/networks/") {
        (
            StatusCode::OK,
            json!({"Name": format!("cpr-slot-{SUFFIX}-net"), "Driver": "bridge", "Internal": true, "Attachable": false, "Options": {"com.docker.network.bridge.name": gateway_host::slots::egress::bridge_name("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d")}, "IPAM": {"Config": [{"Subnet": gateway_host::slots::egress::subnet_for("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d")}]}, "Labels": labels, "Containers": {"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123": {"Name": "gateway-test"}}}),
        )
    } else if uri.contains("/containers/create") {
        (
            StatusCode::CREATED,
            json!({"Id": "fake-container", "Warnings": []}),
        )
    } else if method == Method::PUT && uri.contains("/archive") {
        // 捕获真实归档后结束创建路径，不让测试尝试 sidecar DNS 和健康轮询。
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"message": "fixture upload failure"}),
        )
    } else {
        (StatusCode::NOT_FOUND, json!({"message": "not found"}))
    };
    (
        status,
        [("content-type", "application/json")],
        body.to_string(),
    )
        .into_response()
}

struct FakeEgress {
    events: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl SlotEgressLifecycle for FakeEgress {
    async fn ensure(
        &self,
        _instance: AccountSlotInstanceId,
        _proxy: &OutboundProxy,
    ) -> Result<(), EgressError> {
        self.events.lock().unwrap().push("egress ensure".to_owned());
        Ok(())
    }

    async fn apply_rules(&self, _instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        self.events.lock().unwrap().push("egress apply".to_owned());
        Ok(())
    }

    async fn remove(&self, _instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        self.events.lock().unwrap().push("egress remove".to_owned());
        Ok(())
    }
}

#[tokio::test]
async fn docker_creation_uses_instance_names_and_private_secret_files() {
    let fixture = DockerFixture::start(false).await;
    let desired = super::desired("acct_test", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    assert_eq!(
        fixture.engine.converge(&desired).await.unwrap_err().kind,
        AccountSlotEngineErrorKind::Unavailable
    );
    let capture = fixture.capture.lock().unwrap();
    let events = fixture.events.lock().unwrap();
    let ensure = events
        .iter()
        .position(|event| event == "egress ensure")
        .unwrap();
    let apply = events
        .iter()
        .position(|event| event == "egress apply")
        .unwrap();
    let create = events
        .iter()
        .position(|event| event.contains("/containers/create"))
        .unwrap();
    assert!(ensure < apply && apply < create);
    assert!(
        capture
            .requests
            .iter()
            .all(|(_, uri, _)| !uri.ends_with("/connect"))
    );
    let (_, uri, body) = capture
        .requests
        .iter()
        .find(|(_, uri, _)| uri.contains("/containers/create"))
        .unwrap();
    assert!(uri.contains(&format!("name=cpr-slot-{SUFFIX}")));
    let config: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        config["HostConfig"]["NetworkMode"],
        format!("cpr-slot-{SUFFIX}-net")
    );
    assert_eq!(
        config["HostConfig"]["Binds"][0],
        format!("cpr-slot-{SUFFIX}-data:/var/lib/cpr-slot:rw")
    );
    let config_text = config.to_string();
    assert!(!config_text.contains("acct_test"));
    assert!(!config_text.contains("user:pass"));
    let (_, _, bytes) = capture
        .requests
        .iter()
        .find(|(method, uri, _)| *method == Method::PUT && uri.contains("/archive"))
        .unwrap();
    assert!(!String::from_utf8_lossy(bytes).contains("acct_test"));
    let mut archive = tar::Archive::new(Cursor::new(bytes));
    let mut secrets = std::collections::BTreeMap::new();
    for entry in archive.entries().unwrap() {
        let mut entry = entry.unwrap();
        let path = entry.path().unwrap().to_string_lossy().into_owned();
        if path.ends_with("secrets/auth") || path.ends_with("secrets/proxy") {
            assert_eq!(entry.header().mode().unwrap(), 0o600);
            let mut value = Vec::new();
            entry.read_to_end(&mut value).unwrap();
            secrets.insert(path, value);
        }
    }
    let token = &secrets["var/lib/cpr-slot/secrets/auth"];
    assert!(token.len() >= 32);
    assert!(!config_text.contains(std::str::from_utf8(token).unwrap()));
    assert_eq!(
        secrets["var/lib/cpr-slot/secrets/proxy"],
        b"socks5://user:pass@127.0.0.1:1080"
    );
}

#[tokio::test]
async fn docker_network_creation_pins_bridge_subnet_and_internal_mode() {
    let fixture = DockerFixture::start_with_network_missing(false).await;
    let desired = super::desired("acct_test", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    assert_eq!(
        fixture.engine.converge(&desired).await.unwrap_err().kind,
        AccountSlotEngineErrorKind::Unavailable
    );
    let capture = fixture.capture.lock().unwrap();
    let (_, uri, body) = capture
        .requests
        .iter()
        .find(|(method, uri, _)| *method == Method::POST && uri.contains("/networks/create"))
        .unwrap();
    assert!(uri.contains("/networks/create"));
    let config: Value = serde_json::from_slice(body).unwrap();
    assert_eq!(
        config["Options"]["com.docker.network.bridge.name"],
        gateway_host::slots::egress::bridge_name(desired.instance_id)
    );
    assert_eq!(config["Internal"], true);
    assert_eq!(
        config["IPAM"]["Config"][0]["Subnet"],
        gateway_host::slots::egress::subnet_for(desired.instance_id)
    );
}

#[tokio::test]
async fn docker_rejects_foreign_resources_before_mutation() {
    let fixture = DockerFixture::start(true).await;
    let desired = super::desired("acct_test", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    assert_eq!(
        fixture.engine.converge(&desired).await.unwrap_err().kind,
        AccountSlotEngineErrorKind::Unauthorized
    );
    let capture = fixture.capture.lock().unwrap();
    assert_eq!(capture.requests.len(), 1);
    assert_eq!(capture.requests[0].0, Method::GET);
}

#[tokio::test]
async fn deletion_rejects_foreign_volume_before_any_mutation() {
    let fixture = DockerFixture::start(true).await;
    let id = super::instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    assert_eq!(
        fixture.engine.delete(id).await.unwrap_err().kind,
        AccountSlotEngineErrorKind::Unauthorized
    );
    assert!(
        fixture
            .capture
            .lock()
            .unwrap()
            .requests
            .iter()
            .all(|(method, _, _)| *method == Method::GET)
    );
}
