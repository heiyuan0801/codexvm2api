//! sidecar 启动配置；密钥与代理地址只从只读文件加载。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use url::Url;

const DEFAULT_UPSTREAM_ORIGIN: &str = "https://chatgpt.com";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidecarConfig {
    pub listen: SocketAddr,
    pub auth_file: PathBuf,
    pub proxy_file: PathBuf,
    pub upstream_origin: Url,
}

impl SidecarConfig {
    /// 从非敏感环境变量读取路径和监听地址。
    ///
    /// # Errors
    ///
    /// 路径、监听地址或固定上游 origin 无效时拒绝启动。
    pub fn from_env() -> Result<Self, SidecarConfigError> {
        let listen = std::env::var("CPR_SLOT_LISTEN")
            .unwrap_or_else(|_| "0.0.0.0:8090".to_owned())
            .parse()
            .map_err(|_| SidecarConfigError::InvalidListen)?;
        let auth_file = required_path("CPR_SLOT_AUTH_FILE")?;
        let proxy_file = required_path("CPR_SLOT_PROXY_FILE")?;
        let upstream_origin = std::env::var("CPR_SLOT_UPSTREAM_ORIGIN")
            .unwrap_or_else(|_| DEFAULT_UPSTREAM_ORIGIN.to_owned());
        let upstream_origin = Url::parse(&upstream_origin)
            .ok()
            .filter(|url| {
                matches!(url.scheme(), "http" | "https")
                    && url.host_str().is_some()
                    && url.path() == "/"
                    && url.query().is_none()
                    && url.fragment().is_none()
            })
            .ok_or(SidecarConfigError::InvalidUpstreamOrigin)?;
        Ok(Self {
            listen,
            auth_file,
            proxy_file,
            upstream_origin,
        })
    }

    #[must_use]
    pub fn new(
        listen: SocketAddr,
        auth_file: PathBuf,
        proxy_file: PathBuf,
        upstream_origin: Url,
    ) -> Self {
        Self {
            listen,
            auth_file,
            proxy_file,
            upstream_origin,
        }
    }
}

fn required_path(name: &'static str) -> Result<PathBuf, SidecarConfigError> {
    let value = std::env::var_os(name).ok_or(SidecarConfigError::MissingEnvironment(name))?;
    let path = Path::new(&value);
    if path.as_os_str().is_empty() {
        return Err(SidecarConfigError::MissingEnvironment(name));
    }
    Ok(path.to_path_buf())
}

#[derive(Debug, thiserror::Error)]
pub enum SidecarConfigError {
    #[error("required sidecar environment variable is missing: {0}")]
    MissingEnvironment(&'static str),
    #[error("sidecar listen address is invalid")]
    InvalidListen,
    #[error("sidecar upstream origin is invalid")]
    InvalidUpstreamOrigin,
}
