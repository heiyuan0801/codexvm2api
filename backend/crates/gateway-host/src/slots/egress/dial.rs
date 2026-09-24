//! 通过槽位出口代理建立到目标主机的连接。
//!
//! 槽位流量恢复出目标主机后不直连，而是交给该槽位绑定的出口代理。
//! 支持三种代理：HTTP `CONNECT`、SOCKS5 主机名模式（`socks5`）与
//! SOCKS5 本地解析模式（`socks5h`）。本地解析模式没有可用的解析器时直接失败，
//! 绝不退化成直连，否则槽位流量会泄露宿主真实出口。
//!
//! `https` 代理暂不支持：与代理本身先做 TLS 握手需要 TLS 客户端，而按明文连接
//! 会把 `CONNECT` 与 `Proxy-Authorization` 以明文发给一个期待 TLS 的端口。

use std::io;

use gateway_core::account::OutboundProxy;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

/// 连接目标：主机名 + 端口。端口来自重定向前的目的端口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialTarget {
    pub host: String,
    pub port: u16,
}

/// 单个连接的最大建立时间预算，避免代理无响应时占用重定向槽位。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
/// 代理握手报文长度上限。
const MAX_HANDSHAKE_REPLY: usize = 8 * 1024;

/// 打开一条到 `target` 的连接，出口由 `proxy` 决定。
///
/// # Errors
///
/// 代理不可达、认证失败、代理拒绝目标或超时都会返回错误。
pub async fn connect_via(
    proxy: &OutboundProxy,
    target: &DialTarget,
) -> Result<TcpStream, DialError> {
    let url = Url::parse(proxy.expose_url()).map_err(|_| DialError::InvalidProxy)?;
    // 先校验 scheme 再建连：不支持的方案不能先去连一个不该连的端口。
    validate_scheme(url.scheme())?;
    let authority = endpoint_authority(&url)?;
    let attempt = async {
        let stream = TcpStream::connect(&authority).await?;
        stream.set_nodelay(true)?;
        match url.scheme() {
            "socks5" | "socks5h" => socks5(stream, target, url.scheme() == "socks5h", &url).await,
            "http" => http_connect(stream, target, &url).await,
            // `validate_scheme` 已排除其余方案，这里只是兜底。
            _ => Err(DialError::InvalidProxy),
        }
    };
    tokio::time::timeout(CONNECT_TIMEOUT, attempt)
        .await
        .map_err(|_| DialError::Timeout)?
}

/// 校验代理方案是否可在此模块内使用。
///
/// `https` 代理要先与代理本身做 TLS 握手，本模块没有 TLS 客户端：按明文连接
/// 会把 `CONNECT` 与 `Proxy-Authorization`（Basic 凭据）明文发给一个期待 TLS
/// 的端口——既连不通，又把凭据泄露在链路上。因此直接拒绝，等接入 TLS 客户端
/// 后再支持，而不是退化成明文。
fn validate_scheme(scheme: &str) -> Result<(), DialError> {
    match scheme {
        "socks5" | "socks5h" | "http" => Ok(()),
        "https" => Err(DialError::TlsProxyUnsupported),
        _ => Err(DialError::InvalidProxy),
    }
}

/// 代理监听地址；缺少端口时按 scheme 取默认值。
///
/// `Url::host_str` 对 IPv6 字面量返回已带方括号的 `[::1]`，对域名则不带，
/// 因此这里只补端口，不再自行加括号——否则会拼出 `[[::1]]` 这种无法解析的地址。
fn endpoint_authority(url: &Url) -> Result<String, DialError> {
    let host = url.host_str().ok_or(DialError::InvalidProxy)?;
    let port = url.port_or_known_default().ok_or(DialError::InvalidProxy)?;
    Ok(format!("{host}:{port}"))
}

fn credentials(url: &Url) -> (Option<&str>, Option<&str>) {
    let user = match url.username() {
        "" => None,
        user => Some(user),
    };
    (user, url.password())
}

async fn http_connect(
    mut stream: TcpStream,
    target: &DialTarget,
    url: &Url,
) -> Result<TcpStream, DialError> {
    let authority = match target.host.contains(':') {
        true => format!("[{}]:{}", target.host, target.port),
        false => format!("{}:{}", target.host, target.port),
    };
    let mut request = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Connection: keep-alive\r\n"
    );
    if let (Some(user), Some(password)) = credentials(url) {
        request.push_str(&format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64(user, password)
        ));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).await?;
    let reply = read_until_headers_end(&mut stream).await?;
    let head = std::str::from_utf8(&reply).map_err(|_| DialError::ProxyRejected)?;
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or(DialError::ProxyRejected)?;
    if status != 200 {
        return Err(DialError::ProxyRejected);
    }
    Ok(stream)
}

async fn socks5(
    mut stream: TcpStream,
    target: &DialTarget,
    remote_dns: bool,
    url: &Url,
) -> Result<TcpStream, DialError> {
    let (user, password) = credentials(url);
    let authenticated = user.is_some() && password.is_some();
    // 只声明实际需要的认证方式，避免在没有凭据时被要求认证。
    let methods: &[u8] = if authenticated {
        &[0x00, 0x02]
    } else {
        &[0x00]
    };
    let mut greeting = vec![0x05, methods.len() as u8];
    greeting.extend_from_slice(methods);
    stream.write_all(&greeting).await?;
    let mut choice = [0u8; 2];
    stream.read_exact(&mut choice).await?;
    if choice[0] != 0x05 || choice[1] == 0xFF {
        return Err(DialError::ProxyRejected);
    }
    match choice[1] {
        0x00 => {}
        0x02 => {
            let (Some(user), Some(password)) = (user, password) else {
                return Err(DialError::ProxyRejected);
            };
            if user.len() > 255 || password.len() > 255 {
                return Err(DialError::InvalidProxy);
            }
            let mut auth = vec![0x01, user.len() as u8];
            auth.extend_from_slice(user.as_bytes());
            auth.push(password.len() as u8);
            auth.extend_from_slice(password.as_bytes());
            stream.write_all(&auth).await?;
            let mut reply = [0u8; 2];
            stream.read_exact(&mut reply).await?;
            if reply[1] != 0x00 {
                return Err(DialError::ProxyRejected);
            }
        }
        _ => return Err(DialError::ProxyRejected),
    }
    let mut request = vec![0x05, 0x01, 0x00];
    if remote_dns {
        // socks5h：主机名交给代理解析，避免宿主 DNS 泄露槽位要访问的名字。
        request.push(0x03);
        let host = target.host.as_bytes();
        let length = u8::try_from(host.len()).map_err(|_| DialError::InvalidTarget)?;
        request.push(length);
        request.extend_from_slice(host);
    } else {
        // socks5：本地解析后按 IPv4 提交。解析失败即失败，不做兜底。
        let addresses = tokio::net::lookup_host((target.host.as_str(), target.port))
            .await
            .map_err(|_| DialError::ResolveFailed)?;
        let address = addresses
            .into_iter()
            .find(std::net::SocketAddr::is_ipv4)
            .ok_or(DialError::ResolveFailed)?;
        let octets = match address.ip() {
            std::net::IpAddr::V4(v4) => v4.octets(),
            std::net::IpAddr::V6(_) => return Err(DialError::ResolveFailed),
        };
        request.push(0x01);
        request.extend_from_slice(&octets);
    }
    request.extend_from_slice(&target.port.to_be_bytes());
    stream.write_all(&request).await?;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await?;
    if header[1] != 0x00 {
        return Err(DialError::ProxyRejected);
    }
    // 消费绑定地址，握手结束才是可直接使用的流。
    match header[3] {
        0x01 => {
            let mut rest = [0u8; 6];
            stream.read_exact(&mut rest).await?;
        }
        0x04 => {
            let mut rest = [0u8; 18];
            stream.read_exact(&mut rest).await?;
        }
        0x03 => {
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await?;
            let mut rest = vec![0u8; usize::from(length[0]) + 2];
            stream.read_exact(&mut rest).await?;
        }
        _ => return Err(DialError::ProxyRejected),
    }
    Ok(stream)
}

/// 读到头部结束（`\r\n\r\n`）为止，并限制总量。
async fn read_until_headers_end(stream: &mut TcpStream) -> Result<Vec<u8>, DialError> {
    let mut buffer = Vec::with_capacity(256);
    let mut byte = [0u8; 1];
    while buffer.len() < MAX_HANDSHAKE_REPLY {
        stream.read_exact(&mut byte).await?;
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            return Ok(buffer);
        }
    }
    Err(DialError::ProxyRejected)
}

/// `Basic` 认证用的 base64；只编码 ASCII 凭据，避免引入额外依赖。
fn base64(user: &str, password: &str) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{user}:{password}");
    let bytes = raw.as_bytes();
    let mut encoded = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut block = [0u8; 3];
        block[..chunk.len()].copy_from_slice(chunk);
        let bits = u32::from(block[0]) << 16 | u32::from(block[1]) << 8 | u32::from(block[2]);
        encoded.push(ALPHABET[(bits >> 18) as usize & 0x3F] as char);
        encoded.push(ALPHABET[(bits >> 12) as usize & 0x3F] as char);
        encoded.push(match chunk.len() > 1 {
            true => ALPHABET[(bits >> 6) as usize & 0x3F] as char,
            false => '=',
        });
        encoded.push(match chunk.len() > 2 {
            true => ALPHABET[bits as usize & 0x3F] as char,
            false => '=',
        });
    }
    encoded
}

/// 双向拷贝；任一侧 EOF 即结束整条连接。
pub async fn relay<A, B>(mut left: A, mut right: B) -> io::Result<()>
where
    A: AsyncRead + AsyncWrite + Unpin,
    B: AsyncRead + AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(&mut left, &mut right)
        .await
        .map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DialError {
    #[error("outbound proxy is unusable")]
    InvalidProxy,
    #[error("https outbound proxies are not supported yet")]
    TlsProxyUnsupported,
    #[error("connection target is unusable")]
    InvalidTarget,
    #[error("local name resolution failed")]
    ResolveFailed,
    #[error("outbound proxy rejected the connection")]
    ProxyRejected,
    #[error("outbound proxy timed out")]
    Timeout,
    #[error("outbound proxy I/O failed")]
    Io,
}

impl From<io::Error> for DialError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_vectors() {
        assert_eq!(base64("user", "pass"), "dXNlcjpwYXNz");
        assert_eq!(base64("a", "b"), "YTpi");
        assert_eq!(base64("ab", "c"), "YWI6Yw==");
    }

    #[test]
    fn endpoint_authority_carries_the_proxy_port() {
        let url = Url::parse("http://proxy.example:8080").unwrap();
        assert_eq!(endpoint_authority(&url).unwrap(), "proxy.example:8080");
    }

    #[test]
    fn endpoint_authority_brackets_ipv6_hosts_exactly_once() {
        // `url` 的 `Host::Ipv6` 渲染带方括号，而 `Host::Domain` 不带；
        // 两种形态展开成 authority 时都必须恰好一层方括号。
        let url = Url::parse("socks5h://[::1]:1080").unwrap();
        let authority = endpoint_authority(&url).unwrap();
        assert_eq!(authority, "[::1]:1080");
        assert!(!authority.contains("[["));

        let url = Url::parse("socks5h://[2001:db8::1]:1080").unwrap();
        assert_eq!(endpoint_authority(&url).unwrap(), "[2001:db8::1]:1080");
    }
}
