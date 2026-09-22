//! Ready 账号槽位的内部转发协议。

use gateway_core::account::AccountSlotRoute;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde::Serialize;
use serde_json::Value;

use super::{CodexClientError, CodexClientResult};

#[derive(Serialize)]
struct SlotForwardRequest<'a> {
    path: &'static str,
    headers: Vec<(&'a str, &'a [u8])>,
    body: &'a Value,
}

/// 将已规范化的 Codex HTTP 请求交给账号槽位；外层认证不会进入上游 header 集。
pub(super) async fn send_slot_request(
    client: &reqwest::Client,
    route: &AccountSlotRoute,
    headers: &HeaderMap,
    body: &Value,
) -> CodexClientResult<reqwest::Response> {
    // 保留同名扩展头的所有值与原始字节，不能经字符串 map 覆盖或转码。
    let headers = headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_bytes()))
        .collect();
    let mut authorization = Vec::with_capacity(7 + route.bearer_token().expose_to_provider().len());
    authorization.extend_from_slice(b"Bearer ");
    authorization.extend_from_slice(route.bearer_token().expose_to_provider());
    let authorization = HeaderValue::from_bytes(&authorization)?;
    client
        .post(route.endpoint().clone())
        .header(AUTHORIZATION, authorization)
        .json(&SlotForwardRequest {
            path: "/backend-api/codex/responses",
            headers,
            body,
        })
        .send()
        .await
        .map_err(CodexClientError::Http)
}
