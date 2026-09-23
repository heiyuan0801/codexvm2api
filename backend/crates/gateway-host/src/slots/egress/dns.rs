//! 槽位内建 DNS 视图。
//!
//! 槽位容器的 53 端口被重定向到本模块。转发侧的私有名字在这里解析：
//! 返回合成地址，让槽位把连接送到宿主，再由传输层按 SNI 重新选择出口；
//! 其余名字交给宿主解析器，槽位容器本身不参与任何真实 DNS 查询。

use std::net::{IpAddr, Ipv4Addr};

/// 合成地址使用的保留网段，落在 RFC 5737 文档地址内，不会与真实主机冲突。
pub const SYNTHETIC_RANGE: Ipv4Addr = Ipv4Addr::new(203, 0, 113, 0);
/// 需要被接管并合成地址的后缀。
const INTERCEPT_SUFFIXES: &[&str] = &[
    "openai.com",
    "chatgpt.com",
    "oaistatic.com",
    "oaiusercontent.com",
];
/// 合成记录 TTL，短 TTL 让地址变化能快速生效。
pub const SYNTHETIC_TTL: u32 = 60;
const MAX_MESSAGE: usize = 512;

/// DNS 问题段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub name: String,
    pub qtype: u16,
    pub qclass: u16,
    /// 问题段在报文中的结束偏移，供应答原样回填。
    pub end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsError {
    /// 报文不完整或格式非法。
    Malformed,
    /// 非标准查询（响应、多问题、非 IN 类、操作码非 QUERY）。
    Unsupported,
}

/// 判断该名字是否由槽位视图接管。
#[must_use]
pub fn intercepts(name: &str) -> bool {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    INTERCEPT_SUFFIXES
        .iter()
        .any(|suffix| name == *suffix || name.ends_with(&format!(".{suffix}")))
}

/// 为被接管的名字合成一个稳定地址。
///
/// 同一名字始终得到同一地址，使槽位侧的连接复用不会因地址抖动而失效。
#[must_use]
pub fn synthetic_address(name: &str) -> Ipv4Addr {
    let name = name.trim_end_matches('.').to_ascii_lowercase();
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in name.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // 网络段 203.0.113.0/24，仅取低 8 位，避开发送者地址本身。
    let octet = u8::try_from(hash % 254 + 1).unwrap_or(1);
    Ipv4Addr::new(
        SYNTHETIC_RANGE.octets()[0],
        SYNTHETIC_RANGE.octets()[1],
        SYNTHETIC_RANGE.octets()[2],
        octet,
    )
}

/// 解析报文头部与首个问题段。
///
/// # Errors
///
/// 报文截断返回 [`DnsError::Malformed`]，报文类型不受支持返回 [`DnsError::Unsupported`]。
pub fn parse_question(message: &[u8]) -> Result<Question, DnsError> {
    let header = message.get(..12).ok_or(DnsError::Malformed)?;
    let flags = u16::from_be_bytes([header[2], header[3]]);
    let questions = u16::from_be_bytes([header[4], header[5]]);
    let answers = u16::from_be_bytes([header[6], header[7]]);
    // QR=1 是响应；操作码与问题数必须是最常见的形式，其余不在此处处理。
    if flags & 0x8000 != 0 || questions != 1 || answers != 0 || flags & 0x7800 != 0 {
        return Err(DnsError::Unsupported);
    }
    let mut offset = 12;
    let mut labels = Vec::new();
    loop {
        let length = usize::from(*message.get(offset).ok_or(DnsError::Malformed)?);
        offset += 1;
        if length == 0 {
            break;
        }
        // 压缩指针只会出现在答案段；问题段里出现即视为异常报文。
        if length & 0xC0 != 0 {
            return Err(DnsError::Malformed);
        }
        let label = message
            .get(offset..offset + length)
            .ok_or(DnsError::Malformed)?;
        labels.push(
            std::str::from_utf8(label)
                .map_err(|_| DnsError::Malformed)?
                .to_owned(),
        );
        offset += length;
        if labels.len() > 32 {
            return Err(DnsError::Malformed);
        }
    }
    let tail = message.get(offset..offset + 4).ok_or(DnsError::Malformed)?;
    Ok(Question {
        name: labels.join("."),
        qtype: u16::from_be_bytes([tail[0], tail[1]]),
        qclass: u16::from_be_bytes([tail[2], tail[3]]),
        end: offset + 4,
    })
}

/// 构造应答报文：`answers` 为去重后的地址列表。
///
/// 保留原查询 ID 与问题段，只写回地址记录，避免槽位侧解析器做额外解析。
#[must_use]
pub fn build_response(
    request: &[u8],
    question: &Question,
    answers: &[IpAddr],
    ttl: u32,
) -> Vec<u8> {
    let mut response = request.get(..12).map(<[u8]>::to_vec).unwrap_or_else(|| {
        let mut header = vec![0u8; 12];
        header[5] = 1;
        header
    });
    // QR=1、AA=1、RD 保留；RCODE=0。
    response[2] = response[2] | 0x80 | 0x04;
    response[3] &= 0xF0;
    response[6] = 0;
    response[7] = 0;
    response[8] = 0;
    response[9] = 0;
    response[10] = 0;
    response[11] = 0;
    let mut trimmed = truncate_to_question(request, question);
    response.append(&mut trimmed);
    let mut written = 0_u16;
    if let Some(answer_count) = u16::try_from(answers.len()).ok().map(|count| count.min(8)) {
        for address in answers.iter().take(usize::from(answer_count)) {
            let (kind, octets): (u16, &[u8]) = match address {
                IpAddr::V4(v4) => (1, &v4.octets()),
                IpAddr::V6(v6) => (28, &v6.octets()),
            };
            if question.qtype != 255 && question.qtype != kind {
                continue;
            }
            if response.len() + 16 + octets.len() > MAX_MESSAGE {
                break;
            }
            response.extend_from_slice(&[0xC0, 0x0C]);
            response.extend_from_slice(&kind.to_be_bytes());
            response.extend_from_slice(&1_u16.to_be_bytes());
            response.extend_from_slice(&ttl.to_be_bytes());
            let length = u16::try_from(octets.len()).unwrap_or(4);
            response.extend_from_slice(&length.to_be_bytes());
            response.extend_from_slice(octets);
            written += 1;
        }
    }
    response[6] = (written >> 8) as u8;
    response[7] = written as u8;
    response
}

/// 保留请求中的问题段原文，保证应答与请求逐字节对应。
fn truncate_to_question(request: &[u8], question: &Question) -> Vec<u8> {
    match request.get(12..question.end) {
        Some(question_section) => question_section.to_vec(),
        None => Vec::new(),
    }
}

/// 一次查询的完整应答流程所需的解析结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// 由槽位视图合成地址。
    Synthetic(Vec<IpAddr>),
    /// 交给宿主解析器。
    Forward,
}

/// 决定某个问题的处理方式。
#[must_use]
pub fn resolve(question: &Question) -> Resolution {
    if !matches!(question.qtype, 1 | 28 | 255) || question.qclass != 1 {
        // 非地址类查询没有出口可言，直接空应答。
        return Resolution::Synthetic(Vec::new());
    }
    if !intercepts(&question.name) {
        return Resolution::Forward;
    }
    match question.qtype {
        28 => Resolution::Synthetic(Vec::new()),
        _ => Resolution::Synthetic(vec![IpAddr::V4(synthetic_address(&question.name))]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(name: &str, qtype: u16) -> Vec<u8> {
        let mut message = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0];
        for label in name.split('.') {
            message.push(label.len() as u8);
            message.extend_from_slice(label.as_bytes());
        }
        message.push(0);
        message.extend_from_slice(&qtype.to_be_bytes());
        message.extend_from_slice(&1_u16.to_be_bytes());
        message
    }

    #[test]
    fn parses_question_and_rejects_responses() {
        let parsed = parse_question(&query("ChatGPT.com", 1)).unwrap();
        assert_eq!(parsed.name, "ChatGPT.com");
        assert_eq!(parsed.qtype, 1);
        assert_eq!(parsed.qclass, 1);
        assert_eq!(parsed.end, 12 + 13 + 4);
        let mut response = query("chatgpt.com", 1);
        response[2] |= 0x80;
        assert_eq!(parse_question(&response), Err(DnsError::Unsupported));
        assert_eq!(parse_question(&[0u8; 4]), Err(DnsError::Malformed));
    }

    #[test]
    fn intercepts_only_owned_names() {
        assert!(intercepts("chatgpt.com"));
        assert!(intercepts("api.chatgpt.com."));
        assert!(!intercepts("notchatgpt.com"));
        assert!(!intercepts("example.com"));
    }

    #[test]
    fn synthetic_addresses_are_stable_and_in_range() {
        let first = synthetic_address("chatgpt.com");
        assert_eq!(first, synthetic_address("chatgpt.com."));
        assert_ne!(first, synthetic_address("api.openai.com"));
        assert_eq!(first.octets()[..3], SYNTHETIC_RANGE.octets()[..3]);
        assert_ne!(first.octets()[3], 0);
    }

    #[test]
    fn response_keeps_query_and_counts_answers() {
        let request = query("chatgpt.com", 1);
        let question = parse_question(&request).unwrap();
        let response = build_response(
            &request,
            &question,
            &[IpAddr::V4(synthetic_address("chatgpt.com"))],
            SYNTHETIC_TTL,
        );
        assert_eq!(response[..2], request[..2]);
        assert_eq!(response[2] & 0x80, 0x80);
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 1);
        assert_eq!(response.len(), request.len() + 16);
    }

    #[test]
    fn forwards_unowned_names() {
        let question = parse_question(&query("example.com", 1)).unwrap();
        assert_eq!(resolve(&question), Resolution::Forward);
        let question = parse_question(&query("api.openai.com", 1)).unwrap();
        assert_eq!(
            resolve(&question),
            Resolution::Synthetic(vec![IpAddr::V4(synthetic_address("api.openai.com"))])
        );
    }
}
