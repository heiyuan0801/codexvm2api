//! 透明转发入口的端到端测试。
//!
//! 每条用例都走真实的 TCP：一侧是被测的 [`handle_connection`]，另一侧是桩出口代理。
//! 除了「转发内容正确」，这里主要钉住失败方向的约定——恢复不出主机、没有出口代理、
//! 代理拒绝、方案不受支持时，连接必须被关闭，且不得产生任何直连流量。

use std::net::SocketAddr;
use std::time::Duration;

use gateway_core::account::OutboundProxy;
use gateway_host::slots::egress::{Protocol, SlotEgressConfig, handle_connection};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// 判定「代理没有收到任何连接」的等待上限；只是给桩留出被误连的时间。
const NO_CONNECTION_WINDOW: Duration = Duration::from_millis(200);

/// 建立一对已连接的本地 TCP：返回 (客户端, 服务端)；服务端交给被测代码。
async fn pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind pair");
    let address = listener.local_addr().expect("pair addr");
    let client = TcpStream::connect(address).await.expect("connect pair");
    let (server, _) = listener.accept().await.expect("accept pair");
    (client, server)
}

/// 桩 HTTP `CONNECT` 代理：记录请求头，按 `status` 应答，成功时回显后续字节。
async fn spawn_http_proxy(status: u16) -> (SocketAddr, oneshot::Receiver<String>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("proxy addr");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let head = read_head(&mut stream).await;
        let _ = sender.send(head);
        let reply = match status {
            200 => "HTTP/1.1 200 Connection Established\r\n\r\n".to_owned(),
            other => format!("HTTP/1.1 {other} Not Today\r\n\r\n"),
        };
        if stream.write_all(reply.as_bytes()).await.is_err() {
            return;
        }
        if status == 200 {
            echo(&mut stream).await;
        }
    });
    (address, receiver)
}

/// 桩 SOCKS5 代理：无认证、接受任意目标，返回收到的连接请求原文。
async fn spawn_socks5_proxy() -> (SocketAddr, oneshot::Receiver<Vec<u8>>) {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind proxy");
    let address = listener.local_addr().expect("proxy addr");
    let (sender, receiver) = oneshot::channel();
    tokio::spawn(async move {
        let Ok((mut stream, _)) = listener.accept().await else {
            return;
        };
        let Some(request) = socks5_handshake(&mut stream).await else {
            return;
        };
        let _ = sender.send(request);
        echo(&mut stream).await;
    });
    (address, receiver)
}

/// 读到头部结束（`\r\n\r\n`）为止，返回原文。
async fn read_head(stream: &mut TcpStream) -> String {
    let mut buffer = Vec::new();
    let mut byte = [0u8; 1];
    while stream.read_exact(&mut byte).await.is_ok() {
        buffer.push(byte[0]);
        if buffer.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&buffer).into_owned()
}

/// 完成 SOCKS5 握手并返回连接请求原文（含 ATYP 与地址、端口）。
async fn socks5_handshake(stream: &mut TcpStream) -> Option<Vec<u8>> {
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).await.ok()?;
    let mut methods = vec![0u8; usize::from(greeting[1])];
    stream.read_exact(&mut methods).await.ok()?;
    // 固定选无认证，让被测代码走最短路径。
    stream.write_all(&[0x05, 0x00]).await.ok()?;
    let mut header = [0u8; 4];
    stream.read_exact(&mut header).await.ok()?;
    let mut request = header.to_vec();
    match header[3] {
        0x01 => {
            let mut rest = [0u8; 6];
            stream.read_exact(&mut rest).await.ok()?;
            request.extend_from_slice(&rest);
        }
        0x04 => {
            let mut rest = [0u8; 18];
            stream.read_exact(&mut rest).await.ok()?;
            request.extend_from_slice(&rest);
        }
        0x03 => {
            // ATYP=域名时，长度字节之后是「域名 + 2 字节端口」，不是 +3：
            // 多读一个字节会让桩一直等一个不会到来的字节，被测侧只能等到连接超时。
            let mut length = [0u8; 1];
            stream.read_exact(&mut length).await.ok()?;
            let mut rest = vec![0u8; usize::from(length[0]) + 2];
            stream.read_exact(&mut rest).await.ok()?;
            request.push(length[0]);
            request.extend_from_slice(&rest);
        }
        _ => return None,
    }
    stream
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await
        .ok()?;
    Some(request)
}

/// 回显，直到任一侧关闭。
async fn echo(stream: &mut TcpStream) {
    let mut buffer = [0u8; 1024];
    loop {
        match stream.read(&mut buffer).await {
            Ok(0) | Err(_) => return,
            Ok(read) => {
                if stream.write_all(&buffer[..read]).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// 构造带 SNI 的最小 ClientHello，与 `sni` 模块的解析器逐字段对应。
fn client_hello(host: &str) -> Vec<u8> {
    let name = host.as_bytes();
    let mut extensions = Vec::new();
    extensions.extend_from_slice(&[0x00, 0x00]);
    extensions.extend_from_slice(&u16::try_from(name.len() + 5).unwrap().to_be_bytes());
    extensions.extend_from_slice(&u16::try_from(name.len() + 3).unwrap().to_be_bytes());
    extensions.push(0x00);
    extensions.extend_from_slice(&u16::try_from(name.len()).unwrap().to_be_bytes());
    extensions.extend_from_slice(name);
    let mut body = vec![0x03, 0x03];
    body.extend_from_slice(&[0u8; 32]);
    body.push(0);
    body.extend_from_slice(&[0x00, 0x02, 0x13, 0x01]);
    body.push(1);
    body.push(0);
    // 长度字段是 2 字节：`usize::to_be_bytes` 会写 8 字节，多出的 6 字节会让解析器
    // 读到一个假长度（这里是 0）并判定「没有 SNI」。
    body.extend_from_slice(&u16::try_from(extensions.len()).unwrap().to_be_bytes());
    body.extend_from_slice(&extensions);
    let mut handshake = vec![0x01];
    handshake.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    handshake.extend_from_slice(&body);
    let mut record = vec![0x16, 0x03, 0x01];
    record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
    record.extend_from_slice(&handshake);
    record
}

/// 非 ClientHello 的握手记录（ServerHello），用于验证 SNI 缺失时的失败方向。
fn server_hello() -> Vec<u8> {
    // record: type=0x16, version=0x0303, length=4；handshake: 0x02 + 3 字节长度。
    vec![0x16, 0x03, 0x03, 0x00, 0x04, 0x02, 0x00, 0x00, 0x00]
}

fn proxy_url(value: &str) -> OutboundProxy {
    OutboundProxy::parse(value).expect("proxy url")
}

/// client 侧应看到对端关闭，且读不到任何字节。
///
/// 关闭有两种表现形式：对端把已收字节读完再关（干净 EOF），或带着未读数据直接关
/// （Windows 发 RST，读侧拿到 `ConnectionReset`）。两者都表示「没有回任何东西」，
/// 都是这里期望的结果；只有真的读到字节才算失败。
async fn assert_closed(client: &mut TcpStream) {
    let mut buffer = [0u8; 16];
    match client.read(&mut buffer).await {
        Ok(0) => {}
        Ok(read) => panic!(
            "槽位侧不应收到数据，却读到 {read} 字节: {:02x?}",
            &buffer[..read]
        ),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        Err(error) => panic!("槽位侧应看到连接关闭，却读到 {error}"),
    }
}

#[tokio::test]
async fn tls_flow_recovers_sni_and_relays_through_a_connect_proxy() {
    let (proxy, seen) = spawn_http_proxy(200).await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("http://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Tls, 443, &config).await });

    let hello = client_hello("chatgpt.com");
    client.write_all(&hello).await.expect("write hello");
    // 握手字节必须被补发到上游，否则上游拿不到完整请求。
    let mut echoed = vec![0u8; hello.len()];
    client.read_exact(&mut echoed).await.expect("read hello");
    assert_eq!(echoed, hello);

    client.write_all(b"ping").await.expect("write payload");
    let mut pong = [0u8; 4];
    client.read_exact(&mut pong).await.expect("read payload");
    assert_eq!(&pong, b"ping");

    let head = seen.await.expect("proxy saw the request");
    assert!(
        head.starts_with("CONNECT chatgpt.com:443 HTTP/1.1"),
        "CONNECT 目标应为 SNI 与 443: {head}"
    );
    server_task.abort();
}

#[tokio::test]
async fn http_flow_recovers_host_and_dials_the_entry_port() {
    let (proxy, seen) = spawn_http_proxy(200).await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("http://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Http, 80, &config).await });

    let request = b"GET /v1/models HTTP/1.1\r\nHost: api.example.com\r\n\r\n";
    client.write_all(request).await.expect("write request");
    let mut echoed = vec![0u8; request.len()];
    client.read_exact(&mut echoed).await.expect("read request");
    assert_eq!(echoed, request);

    let head = seen.await.expect("proxy saw the request");
    assert!(
        head.starts_with("CONNECT api.example.com:80 HTTP/1.1"),
        "CONNECT 目标应为 Host 与入口端口: {head}"
    );
    server_task.abort();
}

#[tokio::test]
async fn socks5h_hands_the_hostname_to_the_proxy() {
    let (proxy, seen) = spawn_socks5_proxy().await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("socks5h://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Tls, 443, &config).await });

    let hello = client_hello("chatgpt.com");
    client.write_all(&hello).await.expect("write hello");
    let mut echoed = vec![0u8; hello.len()];
    client.read_exact(&mut echoed).await.expect("read hello");
    assert_eq!(echoed, hello);
    client.write_all(b"ping").await.expect("write payload");
    let mut pong = [0u8; 4];
    client.read_exact(&mut pong).await.expect("read payload");
    assert_eq!(&pong, b"ping");

    let request = seen.await.expect("proxy saw the request");
    assert_eq!(request[0], 0x05, "版本");
    assert_eq!(request[1], 0x01, "CONNECT");
    assert_eq!(request[3], 0x03, "socks5h 必须按域名提交，避免宿主侧解析");
    assert_eq!(request[4], 11);
    assert_eq!(&request[5..16], b"chatgpt.com");
    assert_eq!(u16::from_be_bytes([request[16], request[17]]), 443);
    server_task.abort();
}

#[tokio::test]
async fn socks5_resolves_locally_and_sends_an_ipv4_literal() {
    let (proxy, seen) = spawn_socks5_proxy().await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("socks5://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Http, 80, &config).await });

    let request_line = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    client.write_all(request_line).await.expect("write request");
    let mut echoed = vec![0u8; request_line.len()];
    client.read_exact(&mut echoed).await.expect("read request");
    assert_eq!(echoed, request_line);
    client.write_all(b"ping").await.expect("write payload");
    let mut pong = [0u8; 4];
    client.read_exact(&mut pong).await.expect("read payload");
    assert_eq!(&pong, b"ping");

    let request = seen.await.expect("proxy saw the request");
    assert_eq!(request[3], 0x01, "socks5 必须先本地解析再提交地址");
    assert_eq!(&request[4..8], &[127, 0, 0, 1]);
    assert_eq!(u16::from_be_bytes([request[8], request[9]]), 80);
    server_task.abort();
}

#[tokio::test]
async fn missing_proxy_closes_without_dialling() {
    let (mut client, server) = pair().await;
    let server_task = tokio::spawn(async move {
        handle_connection(
            server,
            Protocol::Tls,
            443,
            &SlotEgressConfig { proxy: None },
        )
        .await
    });

    client
        .write_all(&client_hello("chatgpt.com"))
        .await
        .expect("write hello");
    assert_closed(&mut client).await;
    assert!(server_task.await.expect("join").is_ok());
}

#[tokio::test]
async fn unrecoverable_host_closes_and_never_reaches_the_proxy() {
    let (proxy, seen) = spawn_http_proxy(200).await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("http://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Tls, 443, &config).await });

    client
        .write_all(&server_hello())
        .await
        .expect("write server hello");
    assert_closed(&mut client).await;
    assert!(
        tokio::time::timeout(NO_CONNECTION_WINDOW, seen)
            .await
            .is_err(),
        "恢复不出主机时不得连出"
    );
    server_task.abort();
}

#[tokio::test]
async fn proxy_rejection_closes_the_connection() {
    let (proxy, seen) = spawn_http_proxy(502).await;
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("http://{proxy}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Tls, 443, &config).await });

    client
        .write_all(&client_hello("chatgpt.com"))
        .await
        .expect("write hello");
    assert_closed(&mut client).await;
    let head = seen.await.expect("proxy saw the request");
    assert!(head.starts_with("CONNECT chatgpt.com:443"), "{head}");
    assert!(server_task.await.expect("join").is_ok());
}

#[tokio::test]
async fn https_proxy_is_refused_before_any_connection_is_made() {
    // 桩只负责提供端口，不参与握手；被测代码必须在建连之前就拒绝该方案。
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let config = SlotEgressConfig {
        proxy: Some(proxy_url(&format!("https://{address}"))),
    };
    let (mut client, server) = pair().await;
    let server_task =
        tokio::spawn(async move { handle_connection(server, Protocol::Tls, 443, &config).await });

    client
        .write_all(&client_hello("chatgpt.com"))
        .await
        .expect("write hello");
    assert_closed(&mut client).await;
    assert!(
        tokio::time::timeout(NO_CONNECTION_WINDOW, listener.accept())
            .await
            .is_err(),
        "https 代理必须在建连之前被拒绝，不能明文发出 CONNECT"
    );
    assert!(server_task.await.expect("join").is_ok());
}
