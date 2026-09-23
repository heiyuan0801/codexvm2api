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

use std::sync::Arc;

use async_trait::async_trait;
use gateway_core::account::{AccountSlotInstanceId, OutboundProxy};
use tokio::sync::Mutex;

pub use dial::{DialError, DialTarget, connect_via, relay};
pub use dns::{DnsError, Question, Resolution};
pub use iptables::{CommandIptables, Iptables, IptablesError, forward_parent};
pub use manager::SlotEgressManager;
pub use net::{
    BRIDGE_PREFIX, SUBNET_POOL, bridge_name, gateway_of, parse_ipv4, parse_ipv4_subnet, subnet_for,
};
pub use plan::{EgressTarget, ForwardParent, RedirectPorts, Rule, RulePlan};
pub use sni::{HandshakeProgress, parse_client_hello, parse_http_host, sanitize_host};
pub use transport::{Protocol, SlotEgress, SlotEgressConfig, handle_connection};

/// 容器生命周期所需的出网操作。
///
/// 生产实现持有一个 [`SlotEgressManager`]，测试实现可以替换端口监听和
/// `iptables`，从而在没有 Linux 网桥的环境中仍能验证 Docker 操作顺序。
#[async_trait]
pub trait SlotEgressLifecycle: Send + Sync {
    async fn ensure(
        &self,
        instance: AccountSlotInstanceId,
        proxy: &OutboundProxy,
    ) -> Result<(), EgressError>;

    async fn apply_rules(&self, instance: AccountSlotInstanceId) -> Result<(), EgressError>;

    async fn remove(&self, instance: AccountSlotInstanceId) -> Result<(), EgressError>;
}

/// `SlotEgressManager` 的并发适配器；Docker 引擎的生命周期方法只持有 `&self`。
pub struct ManagedSlotEgress {
    manager: Mutex<SlotEgressManager>,
}

impl std::fmt::Debug for ManagedSlotEgress {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedSlotEgress")
            .field("manager", &self.manager)
            .finish()
    }
}

impl ManagedSlotEgress {
    #[must_use]
    pub fn new(iptables: Arc<dyn Iptables>) -> Self {
        Self {
            manager: Mutex::new(SlotEgressManager::new(iptables)),
        }
    }
}

#[async_trait]
impl SlotEgressLifecycle for ManagedSlotEgress {
    async fn ensure(
        &self,
        instance: AccountSlotInstanceId,
        proxy: &OutboundProxy,
    ) -> Result<(), EgressError> {
        self.manager.lock().await.ensure(instance, proxy).await
    }

    async fn apply_rules(&self, instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        self.manager.lock().await.apply_rules(instance).await
    }

    async fn remove(&self, instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        self.manager.lock().await.remove(instance).await
    }
}

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
