//! Provider 账号独立执行槽位的数据库中立事实。

use std::fmt;
use std::num::NonZeroU64;
use std::str::FromStr;

use async_trait::async_trait;
use chrono_tz::Tz;
use uuid::Uuid;

use crate::error::StoreError;
use crate::validation::IdentifierError;

use super::ProviderAccountId;

/// 不包含账号信息、可安全用于 Docker 资源命名的稳定槽位 ID。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountSlotInstanceId(Uuid);

impl AccountSlotInstanceId {
    /// 生成单调 UUIDv7 槽位 ID。
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// 从数据库 UUID 构造槽位 ID。
    #[must_use]
    pub const fn from_uuid(value: Uuid) -> Self {
        Self(value)
    }

    /// 解析持久化的 `slot_<uuid>` 标识。
    ///
    /// # Errors
    ///
    /// 缺少前缀或 UUID 无效时返回格式错误。
    pub fn parse(value: &str) -> Result<Self, IdentifierError> {
        let raw = value
            .strip_prefix("slot_")
            .ok_or(IdentifierError::MissingPrefix { expected: "slot_" })?;
        Uuid::parse_str(raw)
            .map(Self)
            .map_err(|_| IdentifierError::InvalidFormat)
    }

    /// 返回无前缀 UUID，供 PostgreSQL uuid 字段使用。
    #[must_use]
    pub const fn uuid(self) -> Uuid {
        self.0
    }
}

impl fmt::Display for AccountSlotInstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "slot_{}", self.0.hyphenated())
    }
}

/// 槽位期望配置代次。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AccountSlotGeneration(NonZeroU64);

impl AccountSlotGeneration {
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        match NonZeroU64::new(value) {
            Some(value) => Some(Self(value)),
            None => None,
        }
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0.get()
    }
}

/// 容器重建后仍保持不变的软件设备身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSlotIdentity {
    hostname: String,
    machine_id: String,
    installation_id: String,
    timezone: String,
}

impl AccountSlotIdentity {
    /// 校验 Linux hostname、machine-id、安装 UUID 和 IANA 时区。
    ///
    /// # Errors
    ///
    /// 任一字段不满足稳定格式时返回格式错误。
    pub fn new(
        hostname: String,
        machine_id: String,
        installation_id: String,
        timezone: String,
    ) -> Result<Self, IdentifierError> {
        if !valid_hostname(&hostname)
            || machine_id.len() != 32
            || !machine_id.bytes().all(|byte| byte.is_ascii_hexdigit())
            || Uuid::parse_str(&installation_id).is_err()
            || Tz::from_str(&timezone).is_err()
        {
            return Err(IdentifierError::InvalidFormat);
        }
        Ok(Self {
            hostname,
            machine_id: machine_id.to_ascii_lowercase(),
            installation_id,
            timezone,
        })
    }

    #[must_use]
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    #[must_use]
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    #[must_use]
    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }

    #[must_use]
    pub fn timezone(&self) -> &str {
        &self.timezone
    }
}

/// 已持久化的账号槽位期望状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderAccountSlot {
    account_id: ProviderAccountId,
    enabled: bool,
    instance_id: AccountSlotInstanceId,
    identity: AccountSlotIdentity,
    generation: AccountSlotGeneration,
    outbound_proxy: Option<super::OutboundProxy>,
}

impl ProviderAccountSlot {
    #[must_use]
    pub const fn new(
        account_id: ProviderAccountId,
        enabled: bool,
        instance_id: AccountSlotInstanceId,
        identity: AccountSlotIdentity,
        generation: AccountSlotGeneration,
    ) -> Self {
        Self {
            account_id,
            enabled,
            instance_id,
            identity,
            generation,
            outbound_proxy: None,
        }
    }

    #[must_use]
    pub fn with_outbound_proxy(mut self, proxy: Option<super::OutboundProxy>) -> Self {
        self.outbound_proxy = proxy;
        self
    }

    #[must_use]
    pub const fn outbound_proxy(&self) -> Option<&super::OutboundProxy> {
        self.outbound_proxy.as_ref()
    }

    #[must_use]
    pub const fn account_id(&self) -> &ProviderAccountId {
        &self.account_id
    }

    #[must_use]
    pub const fn enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub const fn instance_id(&self) -> AccountSlotInstanceId {
        self.instance_id
    }

    #[must_use]
    pub const fn identity(&self) -> &AccountSlotIdentity {
        &self.identity
    }

    #[must_use]
    pub const fn generation(&self) -> AccountSlotGeneration {
        self.generation
    }
}

/// Host 和 Provider 读取账号槽位期望状态的持久化端口。
#[async_trait]
pub trait ProviderAccountSlotStore: Send + Sync {
    async fn get_account_slot(
        &self,
        account_id: &ProviderAccountId,
    ) -> Result<Option<ProviderAccountSlot>, StoreError>;

    async fn list_account_slots(&self) -> Result<Vec<ProviderAccountSlot>, StoreError>;

    /// 只返回管理员明确请求删除的槽位，停止或孤儿状态不能推导为删除授权。
    async fn pending_slot_deletions(&self) -> Result<Vec<AccountSlotInstanceId>, StoreError>;

    /// Docker 容器、网络和数据卷全部清理成功后，幂等完成删除。
    async fn complete_slot_deletion(&self, id: AccountSlotInstanceId) -> Result<(), StoreError>;
}

/// Provider 调用 sidecar 所需的每槽认证值。
#[derive(Clone, PartialEq, Eq)]
pub struct AccountSlotBearerToken(Vec<u8>);

impl AccountSlotBearerToken {
    /// # Errors
    ///
    /// token 熵不足或超过内部协议上限时拒绝。
    pub fn new(value: Vec<u8>) -> Result<Self, IdentifierError> {
        if !(32..=4096).contains(&value.len()) {
            return Err(IdentifierError::InvalidFormat);
        }
        Ok(Self(value))
    }

    #[must_use]
    pub fn expose_to_provider(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for AccountSlotBearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AccountSlotBearerToken(<redacted>)")
    }
}

/// Ready 槽位的私有网络地址与认证材料。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSlotRoute {
    endpoint: url::Url,
    bearer_token: AccountSlotBearerToken,
}

impl AccountSlotRoute {
    #[must_use]
    pub const fn new(endpoint: url::Url, bearer_token: AccountSlotBearerToken) -> Self {
        Self {
            endpoint,
            bearer_token,
        }
    }

    #[must_use]
    pub const fn endpoint(&self) -> &url::Url {
        &self.endpoint
    }

    #[must_use]
    pub const fn bearer_token(&self) -> &AccountSlotBearerToken {
        &self.bearer_token
    }
}

/// 管理端可见的脱敏槽位状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSlotState {
    Disabled,
    GlobalDisabled,
    Starting,
    Ready,
    Degraded,
}

impl AccountSlotState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::GlobalDisabled => "global-disabled",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
        }
    }
}

/// Provider 查询 Ready 槽位的进程内运行时端口。
pub trait AccountSlotRuntime: Send + Sync {
    fn route_for_slot(&self, slot: &ProviderAccountSlot) -> Option<AccountSlotRoute> {
        self.route(slot.account_id())
    }
    fn status(&self, slot: &ProviderAccountSlot) -> AccountSlotState {
        if self.route_for_slot(slot).is_some() {
            AccountSlotState::Ready
        } else {
            AccountSlotState::Starting
        }
    }

    fn route(&self, account_id: &ProviderAccountId) -> Option<AccountSlotRoute>;
}

fn valid_hostname(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && !value.starts_with('-')
        && !value.ends_with('-')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}
