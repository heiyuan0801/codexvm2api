//! 使用与 Provider 请求一致的显式代理协议，执行有超时和响应大小限制的出口测试。

use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::{Duration, Instant},
};

use async_trait::async_trait;
use gateway_admin::{model::proxies::ProxyTestResult, ports::proxy::ProxyProbe};
use gateway_core::account::OutboundProxy;
use serde::Deserialize;

enum ProbeStrategy {
    Single(String),
    Dual {
        ipv4_endpoint: String,
        ipv6_endpoint: String,
    },
}

pub struct HttpProxyProbe {
    strategy: ProbeStrategy,
    build_client: Arc<ProxyClientBuilder>,
    location_endpoint: Option<String>,
}

type ProxyClientBuilder =
    dyn Fn(reqwest::ClientBuilder) -> Result<reqwest::Client, &'static str> + Send + Sync;

impl Default for HttpProxyProbe {
    fn default() -> Self {
        // 分别向 IPv4 和 IPv6 专用端点并发探测，以获取真实的双栈出口地址。
        Self::new_dual(
            "https://api.ipify.org?format=json",
            "https://api6.ipify.org?format=json",
        )
        .with_location_endpoint("http://ip-api.com/json")
    }
}

impl HttpProxyProbe {
    #[must_use]
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            strategy: ProbeStrategy::Single(endpoint.into()),
            build_client: Arc::new(|builder| builder.build().map_err(|_| "无法创建代理连接")),
            location_endpoint: None,
        }
    }

    #[must_use]
    pub fn new_dual(ipv4_endpoint: impl Into<String>, ipv6_endpoint: impl Into<String>) -> Self {
        Self {
            strategy: ProbeStrategy::Dual {
                ipv4_endpoint: ipv4_endpoint.into(),
                ipv6_endpoint: ipv6_endpoint.into(),
            },
            build_client: Arc::new(|builder| builder.build().map_err(|_| "无法创建代理连接")),
            location_endpoint: None,
        }
    }

    /// 为生产探测启用基于真实出口 IP 的位置查询；位置查询失败不会让代理测试失败。
    #[must_use]
    pub fn with_location_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.location_endpoint = Some(endpoint.into());
        self
    }

    /// 由组合根注入与 Provider 请求一致的证书信任策略。
    #[must_use]
    pub fn with_client_builder<E>(
        mut self,
        build: impl Fn(reqwest::ClientBuilder) -> Result<reqwest::Client, E> + Send + Sync + 'static,
    ) -> Self {
        self.build_client = Arc::new(move |builder| {
            build(builder).map_err(|_| "无法创建代理连接，请检查证书信任配置")
        });
        self
    }

    async fn exit_ip_at(
        &self,
        proxy: &OutboundProxy,
        endpoint: &str,
    ) -> Result<IpAddr, &'static str> {
        let proxy = reqwest::Proxy::all(proxy.expose_url()).map_err(|_| "代理地址不合法")?;
        let builder = reqwest::Client::builder()
            .no_proxy()
            .proxy(proxy)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(12))
            .redirect(reqwest::redirect::Policy::none());
        let client = (self.build_client)(builder)?;
        let mut response = client.get(endpoint).send().await.map_err(|error| {
            if error.is_timeout() {
                "代理连接超时"
            } else {
                "代理连接失败，请检查地址、认证和网络"
            }
        })?;
        if !response.status().is_success() {
            return Err(
                if response.status() == reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED {
                    "代理认证失败"
                } else {
                    "出口检测服务返回错误状态"
                },
            );
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| "出口检测响应读取失败")?
        {
            if body.len() + chunk.len() > 1024 {
                return Err("出口检测响应过大");
            }
            body.extend_from_slice(&chunk);
        }
        #[derive(Deserialize)]
        struct Response {
            ip: IpAddr,
        }
        serde_json::from_slice::<Response>(&body)
            .map(|response| response.ip)
            .map_err(|_| "出口检测响应不合法")
    }

    async fn location_at(&self, ip: IpAddr) -> Option<gateway_core::account::RequestLocation> {
        let endpoint = self.location_endpoint.as_ref()?;
        if is_reserved_probe_ip(ip) {
            return None;
        }
        let url = format!("{}/{}", endpoint.trim_end_matches('/'), ip);
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(5))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .ok()?;
        let response = client.get(&url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        #[derive(Deserialize)]
        struct LocationResponse {
            status: Option<String>,
            success: Option<bool>,
            #[serde(alias = "countryCode")]
            country_code: Option<String>,
            region: Option<String>,
            #[serde(rename = "regionName")]
            region_name: Option<String>,
            city: Option<String>,
            timezone: Option<serde_json::Value>,
        }
        let payload = response.json::<LocationResponse>().await.ok()?;
        if payload.success == Some(false) || payload.status.as_deref() == Some("fail") {
            return None;
        }
        let timezone = match payload.timezone? {
            serde_json::Value::String(value) => value,
            serde_json::Value::Object(value) => value.get("id")?.as_str()?.to_owned(),
            _ => return None,
        };
        let location = gateway_core::account::RequestLocation {
            country: payload.country_code?.to_ascii_uppercase(),
            region: payload.region.or(payload.region_name)?,
            city: payload.city?,
            timezone: timezone.parse().ok()?,
        };
        location.normalized().ok()
    }
}

fn is_reserved_probe_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(value) => {
            value.is_private()
                || value.is_loopback()
                || value.is_link_local()
                || value.is_unspecified()
                || value.is_multicast()
                || value.octets()[0] == 0
                || matches!(
                    value.octets(),
                    [192, 0, 2, _] | [198, 51, 100, _] | [203, 0, 113, _]
                )
        }
        IpAddr::V6(value) => {
            value.is_loopback()
                || value.is_unspecified()
                || value.is_multicast()
                || value.segments()[0] == 0x2001 && value.segments()[1] == 0x0db8
        }
    }
}

#[async_trait]
impl ProxyProbe for HttpProxyProbe {
    async fn test(&self, proxy: &OutboundProxy) -> ProxyTestResult {
        let started = Instant::now();
        let timeout_limit = Duration::from_secs(15);

        match &self.strategy {
            ProbeStrategy::Single(endpoint) => {
                let result =
                    tokio::time::timeout(timeout_limit, self.exit_ip_at(proxy, endpoint)).await;
                let result = result.unwrap_or(Err("代理连接超时"));
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                match result {
                    Ok(ip) => {
                        let location = self.location_at(ip).await;
                        let (exit_ipv4, exit_ipv6) = match ip {
                            IpAddr::V4(v4) => (Some(v4), None),
                            IpAddr::V6(v6) => (None, Some(v6)),
                        };
                        ProxyTestResult {
                            success: true,
                            latency_ms,
                            exit_ip: Some(ip),
                            exit_ipv4,
                            exit_ipv6,
                            location,
                            message: "连接成功".to_owned(),
                        }
                    }
                    Err(err) => ProxyTestResult {
                        success: false,
                        latency_ms,
                        exit_ip: None,
                        exit_ipv4: None,
                        exit_ipv6: None,
                        location: None,
                        message: err.to_owned(),
                    },
                }
            }
            ProbeStrategy::Dual {
                ipv4_endpoint,
                ipv6_endpoint,
            } => {
                let probe_dual = async {
                    tokio::join!(
                        self.exit_ip_at(proxy, ipv4_endpoint),
                        self.exit_ip_at(proxy, ipv6_endpoint),
                    )
                };
                let result = tokio::time::timeout(timeout_limit, probe_dual).await;
                let latency_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

                match result {
                    Ok((res_v4, res_v6)) => {
                        let exit_ipv4: Option<Ipv4Addr> = match res_v4 {
                            Ok(IpAddr::V4(v4)) => Some(v4),
                            _ => None,
                        };
                        let exit_ipv6: Option<Ipv6Addr> = match res_v6 {
                            Ok(IpAddr::V6(v6)) => Some(v6),
                            _ => None,
                        };
                        let location = exit_ipv4
                            .map(IpAddr::V4)
                            .or_else(|| exit_ipv6.map(IpAddr::V6))
                            .map(|ip| self.location_at(ip));
                        let location = match location {
                            Some(future) => future.await,
                            None => None,
                        };

                        if exit_ipv4.is_some() && exit_ipv6.is_some() {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: exit_ipv4.map(IpAddr::V4),
                                exit_ipv4,
                                exit_ipv6,
                                location,
                                message: "连接成功（双栈可用）".to_owned(),
                            }
                        } else if let Some(v4) = exit_ipv4 {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: Some(IpAddr::V4(v4)),
                                exit_ipv4: Some(v4),
                                exit_ipv6: None,
                                location,
                                message: "连接成功（仅 IPv4）".to_owned(),
                            }
                        } else if let Some(v6) = exit_ipv6 {
                            ProxyTestResult {
                                success: true,
                                latency_ms,
                                exit_ip: Some(IpAddr::V6(v6)),
                                exit_ipv4: None,
                                exit_ipv6: Some(v6),
                                location,
                                message: "连接成功（仅 IPv6）".to_owned(),
                            }
                        } else {
                            let message = res_v4
                                .err()
                                .or(res_v6.err())
                                .unwrap_or("代理连接失败")
                                .to_owned();
                            ProxyTestResult {
                                success: false,
                                latency_ms,
                                exit_ip: None,
                                exit_ipv4: None,
                                exit_ipv6: None,
                                location: None,
                                message,
                            }
                        }
                    }
                    Err(_) => ProxyTestResult {
                        success: false,
                        latency_ms,
                        exit_ip: None,
                        exit_ipv4: None,
                        exit_ipv6: None,
                        location: None,
                        message: "代理连接超时".to_owned(),
                    },
                }
            }
        }
    }
}
