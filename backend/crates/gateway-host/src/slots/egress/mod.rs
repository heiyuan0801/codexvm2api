//! 槽位出网：私有桥接网络 + `iptables` 重定向 + 按 SNI/Host 的透明转发。
//!
//! 每个槽位被放进一张独立网桥，网桥上的出站 TCP/UDP 被 REDIRECT 到本模块，
//! 由本模块从流量中恢复目标主机，再经该槽位绑定的出口代理连出。
//! 任何无法恢复主机的流量都会被断开，因此槽位永远不能绕过出口代理直连。

pub mod dial;
pub mod dns;
pub mod iptables;
pub mod manager;
pub mod net;
pub mod plan;
pub mod sni;
pub mod transport;

pub use dial::{DialError, DialTarget, connect_via, relay};
pub use dns::{DnsError, Question, Resolution};
pub use iptables::{CommandIptables, Iptables, IptablesError, forward_parent};
pub use manager::SlotEgressManager;
pub use net::{
    BRIDGE_PREFIX, LISTEN_ADDR, SUBNET_POOL, bridge_name, gateway_of, parse_ipv4, parse_ipv4_subnet,
    subnet_for,
};
pub use plan::{EgressTarget, ForwardParent, RedirectPorts, Rule, RulePlan};
pub use sni::{HandshakeProgress, parse_client_hello, parse_http_host, sanitize_host};
pub use transport::{Protocol, SlotEgress, SlotEgressConfig, handle_connection};

use thiserror::Error;

/// 槽位出网建立失败的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum EgressError {
    #[error("iptables is unavailable or rejected the rule set")]
    Iptables,
    #[error("the slot bridge network is unusable")]
    Network,
    #[error("the slot redirect ports could not be bound")]
    Ports,
}

impl From<IptablesError> for EgressError {
    fn from(_: IptablesError) -> Self {
        Self::Iptables
    }
}
