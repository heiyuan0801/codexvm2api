use gateway_admin::ports::proxy::ProxyProbe;
use gateway_core::account::OutboundProxy;
use gateway_host::proxy_probe::HttpProxyProbe;
use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{any, header, path},
};

#[tokio::test]
async fn proxy_probe_supports_ipv4_and_ipv6_proxies_and_exit_addresses() {
    for (listen_address, exit_ip) in [
        ("127.0.0.1:0", "203.0.113.8"),
        ("127.0.0.1:0", "2001:db8::8"),
        ("[::1]:0", "203.0.113.8"),
        ("[::1]:0", "2001:db8::8"),
    ] {
        let listener = std::net::TcpListener::bind(listen_address).unwrap();
        let proxy_server = MockServer::builder().listener(listener).start().await;
        Mock::given(header("proxy-authorization", "Basic dXNlcjpwYXNzd29yZA=="))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": exit_ip})))
            .expect(1)
            .mount(&proxy_server)
            .await;
        let proxy =
            OutboundProxy::parse(&format!("http://user:password@{}", proxy_server.address()))
                .unwrap();
        let result = HttpProxyProbe::new("http://unresolvable.invalid/ip")
            .test(&proxy)
            .await;
        assert!(
            result.success,
            "{listen_address} -> {exit_ip}: {}",
            result.message
        );
        assert_eq!(result.exit_ip.unwrap().to_string(), exit_ip);
    }
}

#[tokio::test]
async fn proxy_probe_enriches_exit_ip_with_location() {
    let proxy_server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "8.8.8.8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;
    let location_server = MockServer::start().await;
    Mock::given(path("/8.8.8.8"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "status": "success",
            "countryCode": "US",
            "regionName": "California",
            "city": "Mountain View",
            "timezone": "America/Los_Angeles"
        })))
        .expect(1)
        .mount(&location_server)
        .await;

    let result = HttpProxyProbe::new(proxy_server.uri())
        .with_location_endpoint(location_server.uri())
        .test(&OutboundProxy::parse(&proxy_server.uri()).unwrap())
        .await;

    let location = result.location.expect("location");
    assert_eq!(location.country, "US");
    assert_eq!(location.city, "Mountain View");
    assert_eq!(location.timezone.name(), "America/Los_Angeles");
}

#[tokio::test]
async fn proxy_probe_rejects_auth_errors_redirects_and_invalid_or_oversized_responses() {
    for response in [
        ResponseTemplate::new(407),
        ResponseTemplate::new(302).insert_header("Location", "http://127.0.0.1/"),
        ResponseTemplate::new(200).set_body_json(json!({"ip": "not-an-ip"})),
        ResponseTemplate::new(200).set_body_string("a".repeat(1025)),
    ] {
        let proxy_server = MockServer::start().await;
        Mock::given(any())
            .respond_with(response)
            .expect(1)
            .mount(&proxy_server)
            .await;
        let result = HttpProxyProbe::new("http://unresolvable.invalid/ip")
            .test(&OutboundProxy::parse(&proxy_server.uri()).unwrap())
            .await;
        assert!(!result.success);
        assert!(result.exit_ip.is_none());
    }
}

#[tokio::test]
async fn unavailable_proxy_never_falls_back_to_direct_connection() {
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip":"203.0.113.8"})))
        .expect(0)
        .mount(&target)
        .await;
    let unused = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = OutboundProxy::parse(&format!("http://{}", unused.local_addr().unwrap())).unwrap();
    drop(unused);
    let result = HttpProxyProbe::new(target.uri()).test(&proxy).await;
    assert!(!result.success);
}

#[tokio::test]
async fn invalid_certificate_configuration_should_not_fall_back_or_expose_details() {
    let target = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&target)
        .await;
    let result = HttpProxyProbe::new(target.uri())
        .with_client_builder(|_| Err("private-certificate-path"))
        .test(&OutboundProxy::parse(&target.uri()).unwrap())
        .await;
    assert!(!result.success);
    assert!(!result.message.contains("private-certificate-path"));
}

#[tokio::test]
async fn dual_stack_proxy_probe_reports_both_addresses_when_available() {
    let proxy_server = MockServer::start().await;
    Mock::given(path("/v4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "203.0.113.8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    Mock::given(path("/v6"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "2001:db8::8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    let proxy = OutboundProxy::parse(&proxy_server.uri()).unwrap();
    let result = HttpProxyProbe::new_dual(
        format!("{}/v4", proxy_server.uri()),
        format!("{}/v6", proxy_server.uri()),
    )
    .test(&proxy)
    .await;

    assert!(result.success);
    assert_eq!(result.exit_ipv4.unwrap().to_string(), "203.0.113.8");
    assert_eq!(result.exit_ipv6.unwrap().to_string(), "2001:db8::8");
    assert!(result.message.contains("双栈可用"));
}

#[tokio::test]
async fn dual_stack_proxy_probe_reports_single_stack_when_only_one_succeeds() {
    let proxy_server = MockServer::start().await;
    Mock::given(path("/v4"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"ip": "203.0.113.8"})))
        .expect(1)
        .mount(&proxy_server)
        .await;

    Mock::given(path("/v6"))
        .respond_with(ResponseTemplate::new(500))
        .expect(1)
        .mount(&proxy_server)
        .await;

    let proxy = OutboundProxy::parse(&proxy_server.uri()).unwrap();
    let result = HttpProxyProbe::new_dual(
        format!("{}/v4", proxy_server.uri()),
        format!("{}/v6", proxy_server.uri()),
    )
    .test(&proxy)
    .await;

    assert!(result.success);
    assert_eq!(result.exit_ipv4.unwrap().to_string(), "203.0.113.8");
    assert!(result.exit_ipv6.is_none());
    assert!(result.message.contains("仅 IPv4"));
}
