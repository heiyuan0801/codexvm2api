use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use gateway_api::admin;
use tower::ServiceExt as _;

use super::super::{AdminTestFixture, AdminTestState};

#[tokio::test]
async fn personal_info_requires_admin_and_a_valid_account_query() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (query, authenticated, expected) in [
        ("?accountId=acct_test", false, StatusCode::UNAUTHORIZED),
        ("", true, StatusCode::BAD_REQUEST),
        ("?accountId=bad", true, StatusCode::BAD_REQUEST),
        (
            "?accountId=acct_test&refresh=true",
            true,
            StatusCode::BAD_REQUEST,
        ),
        // 此夹具未提供账号 Store，合法查询应透传服务不可用，而非绕过查询。
        (
            "?accountId=acct_test",
            true,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let mut request = Request::builder()
            .uri(format!("/api/admin/accounts/personal-info{query}"))
            .header("x-request-id", "req_personal_info");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{query}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}

#[tokio::test]
async fn quota_forecast_requires_admin_and_valid_account_query() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (uri, authenticated, expected) in [
        (
            "/api/admin/accounts/quota-forecast?accountId=acct_test",
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "/api/admin/accounts/quota-forecast",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/admin/accounts/quota-forecast?accountId=bad",
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "/api/admin/accounts/quota-forecast?accountId=acct_test&refresh=true",
            true,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut request = Request::builder()
            .uri(uri)
            .header("x-request-id", "req_forecast");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{uri}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let body = to_bytes(response.into_body(), 8192).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(value["data"].is_null());
        assert!(value["message"].is_string());
    }
}

#[tokio::test]
async fn slot_endpoints_require_admin_and_validate_the_request_contract() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (path, body, authenticated, expected) in [
        (
            "query",
            r#"{"accountIds":["acct_test"]}"#,
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "update",
            r#"{"accountId":"acct_test","enabled":true}"#,
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            "query",
            r#"{"accountIds":[]}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "query",
            r#"{"accountIds":["bad"]}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            "update",
            r#"{"accountId":"acct_test","enabled":true,"endpoint":"http://evil"}"#,
            true,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            "query",
            r#"{"accountIds":["acct_test"]}"#,
            true,
            StatusCode::SERVICE_UNAVAILABLE,
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri(format!("/api/admin/accounts/slots/{path}"))
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "req_slot");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let response = admin::router::<AdminTestState>()
            .with_state(fixture.state())
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), expected, "{path} {body}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
    }
}
