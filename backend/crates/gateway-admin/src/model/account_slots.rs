//! 槽位管理只暴露期望开关与脱敏观测，不包含内部地址或认证值。

use super::Revision;
use gateway_core::account::{AccountSlotState, ProviderAccountId, ProviderAccountSlot};
use gateway_core::routing::ProviderKind;

pub struct SlotAccount {
    pub account_id: ProviderAccountId,
    pub provider: ProviderKind,
    pub has_proxy: bool,
    pub account_enabled: bool,
    pub slot: Option<ProviderAccountSlot>,
}

pub struct SetAccountSlot {
    pub account_id: ProviderAccountId,
    pub enabled: bool,
    pub expected_generation: Option<u64>,
}

pub struct AccountSlotView {
    pub account_id: ProviderAccountId,
    pub enabled: bool,
    pub generation: Option<u64>,
    pub state: AccountSlotState,
    pub reason: Option<&'static str>,
}

pub struct AccountSlotsView {
    pub global_enabled: bool,
    pub items: Vec<AccountSlotView>,
}

pub struct AccountSlotMutation {
    pub config_revision: Revision,
    pub account: SlotAccount,
}

/// 独立槽位的持久化事实，代理凭据不进入管理视图。
pub struct ContainerSlot {
    pub id: gateway_core::account::AccountSlotInstanceId,
    pub name: String,
    pub account_id: Option<ProviderAccountId>,
    pub account_name: Option<String>,
    pub account_enabled: bool,
    pub proxy_id: Option<String>,
    pub proxy_name: Option<String>,
    pub running: bool,
    pub start_requested: bool,
    pub delete_requested: bool,
    pub identity: gateway_core::account::AccountSlotIdentity,
    pub generation: gateway_core::account::AccountSlotGeneration,
}

pub enum ContainerSlotAction {
    Create {
        name: String,
    },
    ConfigureProxy {
        proxy_id: Option<String>,
    },
    Bind {
        account_id: Option<ProviderAccountId>,
    },
    Start,
    Stop,
    Delete,
}

pub struct ContainerSlotCommand {
    pub id: Option<gateway_core::account::AccountSlotInstanceId>,
    pub expected_generation: Option<u64>,
    pub action: ContainerSlotAction,
}

pub struct ContainerSlotMutation {
    pub config_revision: Revision,
    pub affected_accounts: Vec<ProviderAccountId>,
}

pub struct ContainerSlotView {
    pub slot: ContainerSlot,
    pub state: &'static str,
    pub reason: Option<&'static str>,
}

pub struct ContainerSlotsView {
    pub global_enabled: bool,
    pub items: Vec<ContainerSlotView>,
}
