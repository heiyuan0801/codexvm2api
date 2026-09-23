//! 透明转发入口：把 REDIRECT 过来的连接还原成 (主机, 端口) 并按槽位出口转发。
//!
//! 三种入口共用同一套流程：
//! 1. TCP 443 入口读 TLS ClientHello，取 SNI；
//! 2. 其余 TCP 入口读明文 HTTP 请求头，取 `Host`；
//! 3. DNS 入口只处理 UDP 53 查询，被接管的名字返回合成地址，其余转发给宿主解析器。
//!
//! 恢复不出主机名、槽位没有出口代理、代理拒绝连接时一律断开，绝不回落到直连。

use std::io;
use std::time::Duration;

use gateway_core::account::OutboundProxy;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::watch;
use tracing::debug;

use super::dial::{DialTarget, connect_via, relay};
use super::dns::{self, DnsError, SYNTHETIC_TTL};
use super::plan::{HTTP_PORT, TLS_PORT};
use super::sni::{self, HandshakeProgress, MAX_HANDSHAKE_BYTES};

/// 单条连接读取握手报文的时间上限。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// 上游解析器应答等待上限。
const DNS_TIMEOUT: Duration = Duration::from_secs(5);
/// 转发用的 DNS 上游；不读取容器 `resolv.conf`，避免被槽位网络自己影响。
pub const UPSTREAM_DNS: &str = "1.1.1.1:53";
/// 单条 UDP DNS 报文上限。
const MAX_DNS_DATAGRAM: usize = 4096;

/// 入口看到的协议类型，决定从流量中恢复主机名的方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    Tls,
    Http,
}

/// 槽位当前的出口配置；对账更新时通过 watch 推给正在运行的入口。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotEgressConfig {
    /// `None` 表示该槽位未绑定出口代理，所有连接一律断开。
    pub proxy: Option<OutboundProxy>,
}

/// 每个槽位一套的转发入口。
pub struct SlotEgress {
    tls: TcpListener,
    http: TcpListener,
    dns: UdpSocket,
}

impl SlotEgress {
    /// 在槽位网关地址上绑定三个转发入口端口。
    ///
    /// 传 0 作为端口时由内核分配，端口号可从 [`SlotEgress::ports`] 读回；
    /// 这比对账逻辑自己维护端口分配器更可靠——内核只会给出真正空闲的端口。
    ///
    /// `address` 用槽位网桥的网关地址而不是 `0.0.0.0`：REDIRECT 改写后的连接
    /// 一定落在网关地址上，绑定具体地址可以让宿主上其它进程无法直接连到入口，
    /// 也就无法借槽位的出口代理上网。
    ///
    /// # Errors
    ///
    /// 端口被占用、地址不属于本机或无权限绑定时返回错误；
    /// 此时已绑定的监听器会被释放，不会留下半套入口。
    pub async fn bind(
        address: std::net::IpAddr,
        ports: super::plan::RedirectPorts,
    ) -> io::Result<Self> {
        let tls = TcpListener::bind((address, ports.tls)).await?;
        let http = TcpListener::bind((address, ports.http)).await?;
        let dns = UdpSocket::bind((address, ports.dns)).await?;
        Ok(Self { tls, http, dns })
    }

    /// 三个入口实际绑定的端口。
    ///
    /// # Errors
    ///
    /// 监听器已被系统回收时返回错误。
    pub fn ports(&self) -> io::Result<super::plan::RedirectPorts> {
        Ok(super::plan::RedirectPorts {
            tls: self.tls.local_addr()?.port(),
            http: self.http.local_addr()?.port(),
            dns: self.dns.local_addr()?.port(),
        })
    }

    /// 一直服务到配置通道关闭或收到取消信号。
    ///
    /// DNS 出口与被接管的名字无关，因此不参与配置热更新。
    pub async fn serve(
        self,
        config: watch::Receiver<SlotEgressConfig>,
        cancelled: watch::Receiver<bool>,
    ) {
        let mut tls_cancel = cancelled.clone();
        let mut http_cancel = cancelled.clone();
        let mut dns_cancel = cancelled;
        let tls = tokio::spawn(accept_tcp(
            self.tls,
            Protocol::Tls,
            TLS_PORT,
            config.clone(),
            async move {
                let _ = tls_cancel.changed().await;
            },
        ));
        let http = tokio::spawn(accept_tcp(
            self.http,
            Protocol::Http,
            HTTP_PORT,
            config,
            async move {
                let _ = http_cancel.changed().await;
            },
        ));
        let dns = tokio::spawn(serve_dns(self.dns, async move {
            let _ = dns_cancel.changed().await;
        }));
        // TLS 入口的端口恒为 443：被重定向过来的流量目的端口没有变化，
        // 因此不需要从 `RedirectPorts` 再带一份。
        let _ = tokio::join!(tls, http, dns);
    }
}

async fn accept_tcp<F>(
    listener: TcpListener,
    protocol: Protocol,
    port: u16,
    config: watch::Receiver<SlotEgressConfig>,
    cancelled: F,
) where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(cancelled);
    loop {
        tokio::select! {
            biased;
            () = &mut cancelled => return,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { return };
                let config = config.borrow().clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, protocol, port, &config).await {
                        debug!(target: "slot_egress", error = %error, protocol = ?protocol, "slot egress connection ended with an error");
                    }
                });
            }
        }
    }
}

/// 处理一条已接受的 TCP 连接。
///
/// 主机名恢复失败、槽位无出口代理、代理拒绝连接都会直接关闭，不产生任何直连流量。
pub async fn handle_connection(
    mut stream: tokio::net::TcpStream,
    protocol: Protocol,
    port: u16,
    config: &SlotEgressConfig,
) -> Result<(), io::Error> {
    let Some(proxy) = config.proxy.as_ref() else {
        return Ok(());
    };
    let Some((host, pending)) = recover_host(&mut stream, protocol).await? else {
        return Ok(());
    };
    let target = DialTarget {
        host: host.clone(),
        port,
    };
    let mut upstream = match connect_via(proxy, &target).await {
        Ok(upstream) => upstream,
        Err(error) => {
            debug!(target: "slot_egress", host = %host, error = %error, "slot egress dial failed");
            return Ok(());
        }
    };
    // 已读到的握手字节必须补发，否则上游拿不到完整请求。
    upstream.write_all(&pending).await?;
    upstream.flush().await?;
    relay(stream, upstream).await
}

/// 在握手字节中恢复主机名。
///
/// 返回 `None` 表示客户端主动断开、报文无法识别或超时，调用方必须关闭连接。
async fn recover_host(
    stream: &mut tokio::net::TcpStream,
    protocol: Protocol,
) -> Result<Option<(String, Vec<u8>)>, io::Error> {
    let mut buffer = Vec::with_capacity(4096);
    let deadline = tokio::time::Instant::now() + HANDSHAKE_TIMEOUT;
    loop {
        let mut chunk = [0u8; 4096];
        let read = match tokio::time::timeout_at(
            deadline,
            tokio::io::AsyncReadExt::read(stream, &mut chunk),
        )
        .await
        {
            Err(_) => return Ok(None),
            Ok(Ok(0)) => return Ok(None),
            Ok(Ok(read)) => read,
            Ok(Err(error)) => return Err(error),
        };
        buffer.extend_from_slice(&chunk[..read]);
        let progress = match protocol {
            Protocol::Tls => sni::parse_client_hello(&buffer),
            Protocol::Http => sni::parse_http_host(&buffer),
        };
        match progress {
            HandshakeProgress::Complete(Some(host)) => return Ok(Some((host, buffer))),
            HandshakeProgress::Complete(None) => return Ok(None),
            HandshakeProgress::Incomplete => {
                if buffer.len() >= MAX_HANDSHAKE_BYTES {
                    return Ok(None);
                }
            }
        }
    }
}

/// 处理槽位 DNS 查询，直到收到取消信号。
///
/// 单条查询在独立任务里处理，慢查询不会阻塞后续报文。
async fn serve_dns<F>(socket: UdpSocket, cancelled: F)
where
    F: std::future::Future<Output = ()> + Send,
{
    tokio::pin!(cancelled);
    let socket = std::sync::Arc::new(socket);
    let mut buffer = vec![0u8; MAX_DNS_DATAGRAM];
    loop {
        tokio::select! {
            biased;
            () = &mut cancelled => return,
            received = socket.recv_from(&mut buffer) => {
                let Ok((length, peer)) = received else { return };
                let request = buffer[..length].to_vec();
                let socket = std::sync::Arc::clone(&socket);
                tokio::spawn(async move {
                    if let Some(response) = answer_query(&request).await {
                        let _ = socket.send_to(&response, peer).await;
                    }
                });
            }
        }
    }
}

/// 生成一条 DNS 应答；返回 `None` 表示不应答（报文无法处理）。
async fn answer_query(request: &[u8]) -> Option<Vec<u8>> {
    let question = match dns::parse_question(request) {
        Ok(question) => question,
        Err(DnsError::Unsupported) => {
            // 非标准查询直接回 SERVFAIL，避免槽位侧长时间等待。
            return Some(servfail(request));
        }
        Err(DnsError::Malformed) => return None,
    };
    match dns::resolve(&question) {
        dns::Resolution::Synthetic(addresses) => Some(dns::build_response(
            request,
            &question,
            &addresses,
            SYNTHETIC_TTL,
        )),
        dns::Resolution::Forward => match forward(request).await {
            Ok(response) => Some(response),
            Err(error) => {
                tracing::warn!(
                    target: "slot_egress",
                    stage = "dns_forward",
                    error = %error,
                    "slot DNS forward failed"
                );
                Some(servfail(request))
            }
        },
    }
}

/// 把查询原样转发给宿主解析器并返回其应答。
async fn forward(request: &[u8]) -> Result<Vec<u8>, io::Error> {
    let socket = UdpSocket::bind(("0.0.0.0", 0)).await?;
    socket.connect(UPSTREAM_DNS).await?;
    socket.send(request).await?;
    let mut response = vec![0u8; MAX_DNS_DATAGRAM];
    let length = tokio::time::timeout(DNS_TIMEOUT, socket.recv(&mut response))
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "upstream DNS timed out"))??;
    response.truncate(length);
    Ok(response)
}

/// 构造 SERVFAIL 应答：保留查询 ID 与问题段，RCODE=2。
fn servfail(request: &[u8]) -> Vec<u8> {
    let mut response = request.to_vec();
    if response.len() < 12 {
        return vec![0, 0, 0x80, 0x02, 0, 0, 0, 0, 0, 0, 0, 0];
    }
    response[2] |= 0x80;
    response[3] = (response[3] & 0xF0) | 0x02;
    response[6] = 0;
    response[7] = 0;
    response[8] = 0;
    response[9] = 0;
    response[10] = 0;
    response[11] = 0;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn servfail_preserves_query_id_and_question() {
        let request = vec![0xAB, 0xCD, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        let response = servfail(&request);
        assert_eq!(response[..4], [0xAB, 0xCD, 0x81, 0x02]);
        assert_eq!(response.len(), request.len());
    }

    #[test]
    fn short_servfail_is_well_formed() {
        assert_eq!(
            servfail(&[0x01, 0x02]),
            vec![0, 0, 0x80, 0x02, 0, 0, 0, 0, 0, 0, 0, 0]
        );
    }
}
