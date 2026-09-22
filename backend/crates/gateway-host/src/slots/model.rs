//! 槽位资源期望与可恢复运行观测。

use gateway_core::account::{
    AccountSlotGeneration, AccountSlotInstanceId, OutboundProxy, ProviderAccountId,
};

/// Host 对单个账号槽位的收敛输入。
#[derive(Clone, PartialEq, Eq)]
pub struct DesiredAccountSlot {
    pub account_id: ProviderAccountId,
    pub instance_id: AccountSlotInstanceId,
    pub generation: AccountSlotGeneration,
    pub hostname: String,
    pub timezone: String,
    pub outbound_proxy: OutboundProxy,
}

impl std::fmt::Debug for DesiredAccountSlot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DesiredAccountSlot")
            .field("account_id", &self.account_id)
            .field("instance_id", &self.instance_id)
            .field("generation", &self.generation)
            .field("hostname", &self.hostname)
            .field("timezone", &self.timezone)
            .field("outbound_proxy", &"<redacted>")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSlotRuntimeState {
    Starting,
    Ready,
    Degraded,
    Stopped,
}

/// 可从 Docker 和健康检查重建的槽位观测，不是业务事实。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSlotHealth {
    pub account_id: ProviderAccountId,
    pub instance_id: AccountSlotInstanceId,
    pub generation: AccountSlotGeneration,
    pub state: AccountSlotRuntimeState,
    pub reason: Option<&'static str>,
}
