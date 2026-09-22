//! 槽位管理的持久化与运行观测端口。

use super::store::AdminStoreResult;
use crate::model::{
    MutationContext,
    account_slots::{AccountSlotMutation, SetAccountSlot, SlotAccount},
};
use async_trait::async_trait;
use gateway_core::account::{AccountSlotRuntime, ProviderAccountId};

#[async_trait]
pub trait AccountSlotAdminStore: Send + Sync {
    async fn containers(
        &self,
    ) -> AdminStoreResult<Vec<crate::model::account_slots::ContainerSlot>> {
        Err(super::store::AdminStoreError::new(
            super::store::AdminStoreErrorKind::Unavailable,
            "containers",
            "容器管理不可用",
        ))
    }
    async fn mutate_container(
        &self,
        _command: crate::model::account_slots::ContainerSlotCommand,
        _context: &MutationContext,
    ) -> AdminStoreResult<crate::model::account_slots::ContainerSlotMutation> {
        Err(super::store::AdminStoreError::new(
            super::store::AdminStoreErrorKind::Unavailable,
            "containers",
            "容器管理不可用",
        ))
    }

    async fn read(&self, accounts: &[ProviderAccountId]) -> AdminStoreResult<Vec<SlotAccount>>;
    /// 校验 Provider、代理及 generation，并在同一事务提交意图、revision 和审计。
    async fn set(
        &self,
        command: SetAccountSlot,
        context: &MutationContext,
    ) -> AdminStoreResult<AccountSlotMutation>;
}

pub trait AccountSlotControl: AccountSlotRuntime {
    fn globally_enabled(&self) -> bool;
    fn container_state(&self, _id: gateway_core::account::AccountSlotInstanceId) -> &'static str {
        "unknown"
    }
    /// 提交后立即撤销旧路由；周期 worker 在下一轮读取持久化意图。
    fn invalidate(&self, account: &ProviderAccountId);
}
