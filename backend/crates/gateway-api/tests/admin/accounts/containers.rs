use super::super::{AdminTestFixture, AdminTestState};
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use gateway_api::admin;
use tower::ServiceExt;

#[tokio::test]
async fn container_slot_endpoints_require_admin_and_reject_invalid_commands() {
    let fixture = AdminTestFixture::new().await;
    fixture.auth.insert_session("valid-session");
    for (body, authenticated, status) in [
        (
            r#"{"action":"create","name":"one"}"#,
            false,
            StatusCode::UNAUTHORIZED,
        ),
        (
            r#"{"action":"start","id":"bad","expectedGeneration":1}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
        (
            r#"{"action":"start","id":"slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e41"}"#,
            true,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            r#"{"action":"create","name":"one","proxyPassword":"secret"}"#,
            true,
            StatusCode::UNPROCESSABLE_ENTITY,
        ),
        (
            r#"{"action":"bind","id":"slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e41","expectedGeneration":1,"accountId":"bad"}"#,
            true,
            StatusCode::BAD_REQUEST,
        ),
    ] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/admin/containers/update")
            .header(header::CONTENT_TYPE, "application/json")
            .header("x-request-id", "req_container_test");
        if authenticated {
            request = request.header(header::COOKIE, "cpr_session=valid-session");
        }
        let router: Router = admin::router::<AdminTestState>().with_state(fixture.state());
        let response = router
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), status, "{body}");
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        let value = to_bytes(response.into_body(), 8192).await.unwrap();
        assert!(!String::from_utf8_lossy(&value).contains("proxyPassword"));
    }
    let response = admin::router::<AdminTestState>()
        .with_state(fixture.state())
        .oneshot(
            Request::builder()
                .uri("/api/admin/containers")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}
