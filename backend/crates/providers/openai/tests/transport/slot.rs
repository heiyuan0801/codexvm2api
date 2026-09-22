use gateway_core::account::{AccountSlotBearerToken, AccountSlotRoute};
use serde_json::json;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{
    CodexBackendClient, CodexBackendClientTestExt, CodexClientError, codex_request,
    request_context, test_wire_profile,
};

#[tokio::test]
async fn ready_slot_is_the_only_http_destination() {
    let direct = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&direct)
        .await;
    let sidecar = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/internal/v1/forward"))
        .and(header(
            "authorization",
            "Bearer 0123456789abcdef0123456789abcdef",
        ))
        .and(body_json(json!({
            "path": "/backend-api/codex/responses",
            "headers": {
                "accept": "text/event-stream",
                "authorization": "Bearer access-token",
                "chatgpt-account-id": "chatgpt-account",
                "content-type": "application/json",
                "originator": "codex_cli_rs",
                "user-agent": "codex_cli_rs/1.2.3 (Linux 6.8; x86_64) transport-test",
                "version": "1.2.3",
                "x-client-request-id": "req_slot",
                "x-codex-routing-hint": "model=gpt-test",
                "x-openai-internal-codex-responses-lite": "false"
            },
            "body": {
                "model": "gpt-test",
                "instructions": "be brief",
                "input": [],
                "stream": true
            }
        })))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "event: response.completed\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_slot\",\"model\":\"gpt-test\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}\n\n"
                )),
        )
        .expect(1)
        .mount(&sidecar)
        .await;
    let endpoint = reqwest::Url::parse(&format!("{}/internal/v1/forward", sidecar.uri()))
        .expect("slot endpoint");
    let route = AccountSlotRoute::new(
        endpoint,
        AccountSlotBearerToken::new(b"0123456789abcdef0123456789abcdef".to_vec())
            .expect("slot token"),
    );
    let client = CodexBackendClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
        direct.uri(),
        test_wire_profile(),
    )
    .with_slot_route(route);

    let response = client
        .create_response(
            &codex_request("gpt-test", "be brief", Vec::new()),
            request_context("req_slot", Some("chatgpt-account")),
        )
        .await
        .expect("slot response");

    assert!(response.body.contains("resp_slot"));
}

#[tokio::test]
async fn websocket_only_request_fails_before_any_send() {
    let sidecar = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&sidecar)
        .await;
    let endpoint = reqwest::Url::parse(&format!("{}/internal/v1/forward", sidecar.uri()))
        .expect("slot endpoint");
    let route = AccountSlotRoute::new(
        endpoint,
        AccountSlotBearerToken::new(b"0123456789abcdef0123456789abcdef".to_vec())
            .expect("slot token"),
    );
    let client = CodexBackendClient::new(
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .expect("client"),
        "http://127.0.0.1:1",
        test_wire_profile(),
    )
    .with_slot_route(route);
    let mut request = codex_request("gpt-test", "be brief", Vec::new());
    request.use_websocket = true;

    let error = client
        .create_response(
            &request,
            request_context("req_slot_ws", Some("chatgpt-account")),
        )
        .await
        .expect_err("slot must reject websocket-only request");

    assert!(matches!(error, CodexClientError::SlotProtocol));
}
