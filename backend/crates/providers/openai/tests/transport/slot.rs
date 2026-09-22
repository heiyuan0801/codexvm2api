use gateway_core::account::{AccountSlotBearerToken, AccountSlotRoute};
use serde_json::Value;
use wiremock::matchers::{header, method, path};
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
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(concat!(
                    "event: response.completed\n",
                    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_slot\",\"model\":\"gpt-test\",\"status\":\"completed\",\"usage\":{\"input_tokens\":1,\"output_tokens\":1,\"total_tokens\":2}}}\n\n"
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

    let mut request = codex_request("gpt-test", "be brief", Vec::new());
    request.use_websocket = true;
    let response = client
        .create_response(
            &request,
            request_context("req_slot", Some("chatgpt-account")),
        )
        .await
        .expect("slot response");

    assert!(response.body.contains("resp_slot"));
    let requests = sidecar.received_requests().await.expect("slot requests");
    let envelope: Value = serde_json::from_slice(&requests[0].body).expect("envelope");
    assert_eq!(envelope["path"], "/backend-api/codex/responses");
    let headers: Vec<(String, Vec<u8>)> =
        serde_json::from_value(envelope["headers"].clone()).expect("headers");
    assert!(headers.contains(&("authorization".to_owned(), b"Bearer access-token".to_vec())));
    assert!(headers.contains(&("chatgpt-account-id".to_owned(), b"chatgpt-account".to_vec())));
    assert!(headers.contains(&("x-client-request-id".to_owned(), b"req_slot".to_vec())));
    assert_eq!(envelope["body"]["model"], "gpt-test");
    assert_eq!(envelope["body"]["instructions"], "be brief");
    assert_eq!(envelope["body"]["stream"], true);
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
    request.set_previous_response_id(Some("resp_connection_local".to_owned()));
    request.previous_response_scope = Some(
        provider_openai::transport::protocol::responses::PreviousResponseScope::ConnectionLocal,
    );

    let error = client
        .create_response(
            &request,
            request_context("req_slot_ws", Some("chatgpt-account")),
        )
        .await
        .expect_err("slot must reject websocket-only request");

    assert!(matches!(error, CodexClientError::SlotProtocol));
}
