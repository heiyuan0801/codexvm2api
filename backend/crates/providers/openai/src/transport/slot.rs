//! Ready 账号槽位的内部转发协议。

use std::collections::BTreeMap;

use gateway_core::account::AccountSlotRoute;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;

use super::{CodexClientError, CodexClientResult};

#[derive(Serialize)]
struct SlotForwardRequest<'a> {
    path: &'static str,
    headers: BTreeMap<String, String>,
    body: &'a Value,
}

/// 将已规范化的 Codex HTTP 请求交给账号槽位；外层认证不会进入上游 header 集。
pub(super) async fn send_slot_request(
    client: &reqwest::Client,
    route: &AccountSlotRoute,
    path: &'static str,
    headers: &HeaderMap,
    body: &Value,
) -> CodexClientResult<reqwest::Response> {
    let headers = serialize_headers(headers)?;
    let mut authorization = Vec::with_capacity(7 + route.bearer_token().expose_to_provider().len());
    authorization.extend_from_slice(b"Bearer ");
    authorization.extend_from_slice(route.bearer_token().expose_to_provider());
    let authorization = HeaderValue::from_bytes(&authorization)?;
    client
        .post(route.endpoint().clone())
        .header(AUTHORIZATION, authorization)
        .json(&SlotForwardRequest {
            path,
            headers,
            body,
        })
        .send()
        .await
        .map_err(CodexClientError::Http)
}

fn serialize_headers(headers: &HeaderMap) -> CodexClientResult<BTreeMap<String, String>> {
    let mut serialized = BTreeMap::new();
    for (name, value) in headers {
        let value = value.to_str().map_err(|_| CodexClientError::SlotProtocol)?;
        serialized.insert(name.as_str().to_owned(), value.to_owned());
    }
    Ok(serialized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderName, HeaderValue};

    #[test]
    fn envelope_headers_keep_upstream_authorization_separate() {
        let mut headers = HeaderMap::new();
        headers.insert(
            HeaderName::from_static("authorization"),
            HeaderValue::from_static("Bearer upstream-token"),
        );

        let envelope = serialize_headers(&headers).expect("headers");

        assert_eq!(
            envelope.get("authorization").map(String::as_str),
            Some("Bearer upstream-token")
        );
    }
}
