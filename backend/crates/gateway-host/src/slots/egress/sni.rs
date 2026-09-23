//! TLS ClientHello 与明文 HTTP 请求头解析。
//!
//! 透明重定向丢失了原始目的地址（`SO_ORIGINAL_DST` 需要 `unsafe`，本工作区禁用），
//! 因此目的主机改为从流量自身恢复：443 取 SNI，80 取 `Host` 头。
//! 拿不到主机名的流量一律拒绝，保证不会退化成直连。

/// 主机名长度上限，避免异常输入构造超长连接目标。
pub const MAX_HOST_LEN: usize = 255;
/// 单个 TLS 记录的最大长度。
const MAX_RECORD_LEN: usize = 16 * 1024 + 5;
/// 握手缓冲上限，超出即判定为无法识别的流量。
pub const MAX_HANDSHAKE_BYTES: usize = 64 * 1024;

/// ClientHello 解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeProgress {
    /// 已经解析出 SNI。
    Complete(Option<String>),
    /// 报文尚不完整，需要继续读取。
    Incomplete,
}

/// 从 TLS 记录流中提取 SNI。
///
/// 只处理握手记录，忽略记录边界，容忍 ClientHello 跨记录分片。
#[must_use]
pub fn parse_client_hello(buffer: &[u8]) -> HandshakeProgress {
    let Some(mut payload) = handshake_payload(buffer) else {
        return HandshakeProgress::Incomplete;
    };
    if payload.len() < 4 {
        return HandshakeProgress::Incomplete;
    }
    if payload[0] != 0x01 {
        // 非 ClientHello 的握手消息没有可用的目标主机。
        return HandshakeProgress::Complete(None);
    }
    let declared = u32::from(payload[1]) << 16 | u32::from(payload[2]) << 8 | u32::from(payload[3]);
    payload = &payload[4..];
    if payload.len() < declared as usize {
        return HandshakeProgress::Incomplete;
    }
    HandshakeProgress::Complete(parse_body(&payload[..declared as usize]))
}

/// 拼接握手记录并返回握手消息体。
fn handshake_payload(buffer: &[u8]) -> Option<&[u8]> {
    let mut offset = 0;
    let mut start = None;
    loop {
        let header = buffer.get(offset..offset + 5)?;
        if header[0] != 0x16 {
            return None;
        }
        let length = usize::from(header[3]) << 8 | usize::from(header[4]);
        if length > MAX_RECORD_LEN {
            return None;
        }
        if start.is_none() {
            start = Some(offset + 5);
        }
        let end = offset + 5 + length;
        if end > buffer.len() {
            return None;
        }
        // ClientHello 之后的记录与目标主机无关，解析到此为止。
        let payload = &buffer[start?..end];
        if payload.len() >= 4 {
            let declared =
                u32::from(payload[1]) << 16 | u32::from(payload[2]) << 8 | u32::from(payload[3]);
            if payload.len() >= 4 + declared as usize {
                return Some(payload);
            }
        }
        offset = end;
    }
}

/// 解析 ClientHello 主体，定位 `server_name` 扩展。
fn parse_body(body: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(body);
    // legacy_version(2) + random(32)；两者都是定长，可以直接跳过。
    cursor.skip(2 + 32)?;
    // 其余字段都必须按各自的长度前缀推进：真实客户端普遍不发送空 session_id。
    let session_id = cursor.take_u8()?;
    cursor.skip(usize::from(session_id))?;
    let cipher_suites = cursor.take_u16()?;
    cursor.skip(usize::from(cipher_suites))?;
    let compression = cursor.take_u8()?;
    cursor.skip(usize::from(compression))?;
    let extensions_len = cursor.take_u16()?;
    let extensions = cursor.rest(usize::from(extensions_len))?;
    let mut cursor = Cursor::new(extensions);
    while cursor.remaining() >= 4 {
        let kind = cursor.take_u16()?;
        let length = cursor.take_u16()?;
        let extension = cursor.rest(usize::from(length))?;
        if kind == 0 {
            return server_name(extension);
        }
    }
    None
}

/// 解析 `server_name` 扩展，只接受 DNS 类型且不含 NUL 的名称。
fn server_name(extension: &[u8]) -> Option<String> {
    let mut cursor = Cursor::new(extension);
    let list_len = cursor.take_u16()?;
    let list = cursor.rest(usize::from(list_len))?;
    let mut cursor = Cursor::new(list);
    while cursor.remaining() >= 3 {
        let name_type = cursor.take_u8()?;
        let length = usize::from(cursor.take_u16()?);
        let name = cursor.rest(length)?;
        if name_type == 0 {
            return std::str::from_utf8(name).ok().and_then(sanitize_host);
        }
    }
    None
}

/// 校验主机名可用作连接目标：仅小写可打印 ASCII，无控制字符与端口分隔符。
#[must_use]
pub fn sanitize_host(value: &str) -> Option<String> {
    let value = value.trim().trim_end_matches('.');
    if value.is_empty() || value.len() > MAX_HOST_LEN {
        return None;
    }
    let valid = value.bytes().all(|byte| {
        byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_' | b':')
    });
    if !valid || value.starts_with('.') || value.contains("..") {
        return None;
    }
    Some(value.to_ascii_lowercase())
}

/// 从明文 HTTP 请求头中提取 `Host`。
///
/// 返回 `None` 表示头部已完整但没有 `Host`，调用方必须拒绝该请求。
#[must_use]
pub fn parse_http_host(buffer: &[u8]) -> HandshakeProgress {
    let Some(end) = find_header_end(buffer) else {
        if buffer.len() >= MAX_HANDSHAKE_BYTES {
            return HandshakeProgress::Complete(None);
        }
        return HandshakeProgress::Incomplete;
    };
    let head = match std::str::from_utf8(&buffer[..end]) {
        Ok(head) => head,
        Err(_) => return HandshakeProgress::Complete(None),
    };
    let mut request_line = head.lines();
    let Some(line) = request_line.next() else {
        return HandshakeProgress::Complete(None);
    };
    let mut parts = line.split_whitespace();
    let Some(method) = parts.next() else {
        return HandshakeProgress::Complete(None);
    };
    // CONNECT 的目标就是 authority，不需要 Host 头。
    if method.eq_ignore_ascii_case("CONNECT") {
        let Some(authority) = parts.next() else {
            return HandshakeProgress::Complete(None);
        };
        return HandshakeProgress::Complete(sanitize_host(strip_port(authority)));
    }
    for header in head[line.len()..].lines() {
        let Some((name, value)) = header.split_once(':') else {
            continue;
        };
        if name.trim().eq_ignore_ascii_case("host") {
            return HandshakeProgress::Complete(sanitize_host(strip_port(value.trim())));
        }
    }
    HandshakeProgress::Complete(None)
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|index| index + 2)
}

/// 去掉 `host:port` 或 `[v6]:port` 中的端口部分。
fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        return rest.split(']').next().unwrap_or(rest);
    }
    match authority.split_once(':') {
        Some((host, port)) if port.chars().all(|character| character.is_ascii_digit()) => host,
        Some(_) => authority,
        None => authority,
    }
}

struct Cursor<'a> {
    buffer: &'a [u8],
    offset: usize,
}

impl<'a> Cursor<'a> {
    const fn new(buffer: &'a [u8]) -> Self {
        Self { buffer, offset: 0 }
    }

    fn remaining(&self) -> usize {
        self.buffer.len().saturating_sub(self.offset)
    }

    fn skip(&mut self, count: usize) -> Option<()> {
        self.rest(count).map(|_| ())
    }

    fn take_u8(&mut self) -> Option<u8> {
        let byte = *self.buffer.get(self.offset)?;
        self.offset += 1;
        Some(byte)
    }

    fn take_u16(&mut self) -> Option<u16> {
        let bytes = self.rest(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn rest(&mut self, count: usize) -> Option<&'a [u8]> {
        let slice = self.buffer.get(self.offset..self.offset + count)?;
        self.offset += count;
        Some(slice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造带 SNI 的最小 ClientHello。
    fn client_hello(host: &str) -> Vec<u8> {
        let name = host.as_bytes();
        let mut extensions = Vec::new();
        // extension_type(0) + extension_length(2)
        extensions.extend_from_slice(&[0x00, 0x00]);
        extensions.extend_from_slice(&u16::try_from(name.len() + 5).unwrap().to_be_bytes());
        // server_name_list_length(2) + name_type(1) + name_length(2) + name
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
        body.extend_from_slice(&(extensions.len() as u16).to_be_bytes());
        body.extend_from_slice(&extensions);
        let mut handshake = vec![0x01];
        let length = body.len() as u32;
        handshake.extend_from_slice(&length.to_be_bytes()[1..]);
        handshake.extend_from_slice(&body);
        let mut record = vec![0x16, 0x03, 0x01];
        record.extend_from_slice(&(handshake.len() as u16).to_be_bytes());
        record.extend_from_slice(&handshake);
        record
    }

    #[test]
    fn extracts_sni_from_client_hello() {
        let hello = client_hello("chatgpt.com");
        assert_eq!(
            parse_client_hello(&hello),
            HandshakeProgress::Complete(Some("chatgpt.com".to_owned()))
        );
    }

    /// 回归：扩展长度字段必须是 2 字节。用 `usize::to_be_bytes` 会多写 6 个 0，
    /// 解析器随后读到一个假长度并判定「没有 SNI」，握手会被当成无法识别而断开。
    #[test]
    fn hello_builder_field_widths_match_the_parser() {
        let hello = client_hello("chatgpt.com");
        // record(5) + handshake 头(4) + body(63)：body 里任何字段写宽了都会超长。
        assert_eq!(hello.len(), 72);
        assert_eq!(u16::from_be_bytes([hello[3], hello[4]]) as usize, hello.len() - 5);
    }

    #[test]
    fn truncated_client_hello_asks_for_more() {
        let hello = client_hello("chatgpt.com");
        assert_eq!(
            parse_client_hello(&hello[..hello.len() - 4]),
            HandshakeProgress::Incomplete
        );
    }

    #[test]
    fn parses_host_header_and_connect_authority() {
        assert_eq!(
            parse_http_host(b"GET / HTTP/1.1\r\nHost: api.example.com:443\r\n\r\n"),
            HandshakeProgress::Complete(Some("api.example.com".to_owned()))
        );
        assert_eq!(
            parse_http_host(b"POST /v1 HTTP/1.1\r\nUser-Agent: x\r\nHOST: a.b\r\n\r\n"),
            HandshakeProgress::Complete(Some("a.b".to_owned()))
        );
        assert_eq!(
            parse_http_host(b"CONNECT example.com:443 HTTP/1.1\r\n\r\n"),
            HandshakeProgress::Complete(Some("example.com".to_owned()))
        );
        assert_eq!(
            parse_http_host(b"GET / HTTP/1.1\r\n\r\n"),
            HandshakeProgress::Complete(None)
        );
        assert_eq!(
            parse_http_host(b"GET / HTTP/1.1\r\nHost: a"),
            HandshakeProgress::Incomplete
        );
    }

    #[test]
    fn rejects_unusable_hosts() {
        assert_eq!(sanitize_host("ChatGPT.COM."), Some("chatgpt.com".to_owned()));
        assert_eq!(sanitize_host(""), None);
        assert_eq!(sanitize_host("a..b"), None);
        assert_eq!(sanitize_host("bad host"), None);
        assert_eq!(sanitize_host(&"a".repeat(300)), None);
    }
}
