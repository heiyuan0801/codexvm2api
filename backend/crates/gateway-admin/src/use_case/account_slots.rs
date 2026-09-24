//! 账号槽位意图的管理用例。

use super::{map_store_error, publish_committed};
use crate::{
    model::{AdminError, MutationContext, account_slots::*},
    ports::account_slots::{AccountSlotAdminStore, AccountSlotControl},
};
use async_trait::async_trait;
use gateway_core::{
    account::{AccountSlotState, ProviderAccountId},
    runtime::SnapshotControl,
};
use std::sync::Arc;

#[async_trait]
pub trait AccountSlotsService: Send + Sync {
    async fn containers(&self) -> Result<ContainerSlotsView, AdminError>;
    async fn mutate_container(
        &self,
        command: ContainerSlotCommand,
        context: &MutationContext,
    ) -> Result<(), AdminError>;

    async fn read(&self, accounts: Vec<ProviderAccountId>) -> Result<AccountSlotsView, AdminError>;
    async fn set(
        &self,
        command: SetAccountSlot,
        context: &MutationContext,
    ) -> Result<AccountSlotView, AdminError>;
}

pub struct DefaultAccountSlotsService {
    store: Arc<dyn AccountSlotAdminStore>,
    runtime: Arc<dyn AccountSlotControl>,
    snapshot: Arc<dyn SnapshotControl>,
}

impl DefaultAccountSlotsService {
    pub fn new(
        store: Arc<dyn AccountSlotAdminStore>,
        runtime: Arc<dyn AccountSlotControl>,
        snapshot: Arc<dyn SnapshotControl>,
    ) -> Self {
        Self {
            store,
            runtime,
            snapshot,
        }
    }

    fn view(&self, account: SlotAccount) -> AccountSlotView {
        let enabled = account.slot.as_ref().is_some_and(|slot| slot.enabled());
        let state = if !enabled {
            AccountSlotState::Disabled
        } else if !self.runtime.globally_enabled() {
            AccountSlotState::GlobalDisabled
        } else if !account.has_proxy || !account.account_enabled {
            AccountSlotState::Degraded
        } else {
            account
                .slot
                .as_ref()
                .map_or(AccountSlotState::Disabled, |slot| self.runtime.status(slot))
        };
        let reason = match state {
            AccountSlotState::Disabled | AccountSlotState::Ready => None,
            AccountSlotState::GlobalDisabled => Some("全局槽位功能尚未启用"),
            AccountSlotState::Starting => Some("等待槽位容器就绪"),
            AccountSlotState::Degraded if !account.account_enabled => Some("账号已停用"),
            AccountSlotState::Degraded if !account.has_proxy => Some("账号未配置有效代理"),
            AccountSlotState::Degraded => Some("槽位容器不可用，请检查 Docker、镜像和代理配置"),
        };
        AccountSlotView {
            account_id: account.account_id,
            enabled,
            generation: account.slot.map(|slot| slot.generation().get()),
            state,
            reason,
        }
    }
}

#[async_trait]
impl AccountSlotsService for DefaultAccountSlotsService {
    async fn containers(&self) -> Result<ContainerSlotsView, AdminError> {
        let slots = self
            .store
            .containers()
            .await
            .map_err(|e| map_store_error(e, "containers"))?;
        let items = slots
            .into_iter()
            .map(|slot| {
                let (state, reason) = if slot.delete_requested {
                    if !self.runtime.globally_enabled() {
                        ("deleting", Some("清理待执行，请在部署配置中开启容器功能"))
                    } else if self.runtime.container_state(slot.id) == "degraded" {
                        (
                            "delete-failed",
                            Some("清理失败，将自动重试，请检查 Docker 资源归属及占用"),
                        )
                    } else {
                        ("deleting", Some("正在清理容器、独立网络和数据卷"))
                    }
                } else if !slot.running {
                    if slot.start_requested {
                        let state = self.runtime.container_state(slot.id);
                        (
                            state,
                            (state == "unknown").then_some("等待 Docker 状态确认"),
                        )
                    } else {
                        ("not-created", None)
                    }
                } else if !self.runtime.globally_enabled() {
                    ("global-disabled", Some("请在部署配置中开启容器功能"))
                } else if !slot.account_enabled {
                    ("degraded", Some("绑定账号已停用，请在账号管理中启用"))
                } else if let Some(account) = &slot.account_id {
                    let desired = gateway_core::account::ProviderAccountSlot::new(
                        account.clone(),
                        true,
                        slot.id,
                        slot.identity.clone(),
                        slot.generation,
                    );
                    match self.runtime.status(&desired) {
                        AccountSlotState::Ready => ("ready", None),
                        AccountSlotState::Degraded => (
                            "degraded",
                            Some("容器不可用，请检查 Docker、镜像和代理配置"),
                        ),
                        _ => ("starting", Some("等待容器就绪")),
                    }
                } else {
                    ("degraded", Some("尚未绑定账号"))
                };
                let egress = slot.account_id.as_ref().and_then(|account| {
                    let desired = gateway_core::account::ProviderAccountSlot::new(
                        account.clone(),
                        slot.running,
                        slot.id,
                        slot.identity.clone(),
                        slot.generation,
                    );
                    self.runtime.egress_for_slot(&desired)
                });
                ContainerSlotView {
                    slot,
                    egress,
                    state,
                    reason,
                }
            })
            .collect();
        Ok(ContainerSlotsView {
            global_enabled: self.runtime.globally_enabled(),
            items,
        })
    }

    async fn mutate_container(
        &self,
        command: ContainerSlotCommand,
        context: &MutationContext,
    ) -> Result<(), AdminError> {
        if let ContainerSlotAction::Create { name } = &command.action {
            if name.trim().is_empty()
                || name.chars().count() > 100
                || name.chars().any(char::is_control)
            {
                return Err(AdminError::invalid("容器名称需为 1 至 100 个字符"));
            }
        } else if command.id.is_none() || command.expected_generation.is_none() {
            return Err(AdminError::invalid("缺少槽位 ID 或配置代次"));
        }
        if matches!(command.action, ContainerSlotAction::Start) && !self.runtime.globally_enabled()
        {
            return Err(AdminError::invalid("请先在部署配置中开启容器功能"));
        }
        let result = self
            .store
            .mutate_container(command, context)
            .await
            .map_err(|e| map_store_error(e, "containers"))?;
        for account in &result.affected_accounts {
            self.runtime.invalidate(account);
        }
        publish_committed(self.snapshot.as_ref(), result.config_revision).await?;
        Ok(())
    }

    async fn read(&self, accounts: Vec<ProviderAccountId>) -> Result<AccountSlotsView, AdminError> {
        if accounts.is_empty() || accounts.len() > 200 {
            return Err(AdminError::invalid("每次查询 1 至 200 个账号"));
        }
        let rows = self
            .store
            .read(&accounts)
            .await
            .map_err(|error| map_store_error(error, "account slots"))?;
        Ok(AccountSlotsView {
            global_enabled: self.runtime.globally_enabled(),
            items: rows.into_iter().map(|row| self.view(row)).collect(),
        })
    }

    async fn set(
        &self,
        command: SetAccountSlot,
        context: &MutationContext,
    ) -> Result<AccountSlotView, AdminError> {
        let accounts = self
            .store
            .read(std::slice::from_ref(&command.account_id))
            .await
            .map_err(|error| map_store_error(error, "account slots"))?;
        let account = accounts
            .into_iter()
            .next()
            .ok_or_else(|| AdminError::not_found("账号不存在"))?;
        if account.provider.as_str() != "openai" {
            return Err(AdminError::invalid("仅 OpenAI 账号支持独立槽位"));
        }
        if command.enabled && !account.has_proxy {
            return Err(AdminError::invalid("请先为账号配置有效代理"));
        }
        let mutation = self
            .store
            .set(command, context)
            .await
            .map_err(|error| map_store_error(error, "account slots"))?;
        self.runtime.invalidate(&mutation.account.account_id);
        publish_committed(self.snapshot.as_ref(), mutation.config_revision).await?;
        Ok(self.view(mutation.account))
    }
}
