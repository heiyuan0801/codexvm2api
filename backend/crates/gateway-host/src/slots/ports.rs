//! Docker 资源驱动端口，使生命周期策略可在无 Docker 的单元测试中验证。

use async_trait::async_trait;

use super::{AccountSlotHealth, ConvergedAccountSlot, DesiredAccountSlot};
use gateway_core::account::AccountSlotInstanceId;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("account slot engine operation failed: {kind:?}")]
pub struct AccountSlotEngineError {
    pub kind: AccountSlotEngineErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSlotEngineErrorKind {
    Unavailable,
    InvalidState,
    Unauthorized,
}

/// Docker adapter 必须只操作带本应用 owner labels 的精确资源。
#[async_trait]
pub trait AccountSlotEngine: Send + Sync {
    async fn list_owned(&self) -> Result<Vec<AccountSlotHealth>, AccountSlotEngineError>;

    async fn inspect(
        &self,
        instance_id: AccountSlotInstanceId,
    ) -> Result<Option<AccountSlotHealth>, AccountSlotEngineError>;

    async fn converge(
        &self,
        desired: &DesiredAccountSlot,
    ) -> Result<ConvergedAccountSlot, AccountSlotEngineError>;

    async fn stop(&self, instance_id: AccountSlotInstanceId) -> Result<(), AccountSlotEngineError>;
}

#[async_trait]
pub trait AccountSlotDesiredStateSource: Send + Sync {
    async fn list_desired_slots(&self) -> Result<Vec<DesiredAccountSlot>, AccountSlotEngineError>;
}
