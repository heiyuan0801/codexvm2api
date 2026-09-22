//! 将独立槽位的运行意图、出口和账号可用性组合为 Docker 期望状态。

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use gateway_core::account::{ProviderAccountSlotStore, ProviderAccountStore};

use super::{
    AccountSlotDesiredStateSource, AccountSlotEngineError, AccountSlotEngineErrorKind,
    DesiredAccountSlot,
};

pub struct StoredAccountSlotDesiredStateSource {
    slots: Arc<dyn ProviderAccountSlotStore>,
    accounts: Arc<dyn ProviderAccountStore>,
}

impl StoredAccountSlotDesiredStateSource {
    #[must_use]
    pub fn new(
        slots: Arc<dyn ProviderAccountSlotStore>,
        accounts: Arc<dyn ProviderAccountStore>,
    ) -> Self {
        Self { slots, accounts }
    }
}

#[async_trait]
impl AccountSlotDesiredStateSource for StoredAccountSlotDesiredStateSource {
    async fn pending_deletions(
        &self,
    ) -> Result<Vec<gateway_core::account::AccountSlotInstanceId>, AccountSlotEngineError> {
        self.slots
            .pending_slot_deletions()
            .await
            .map_err(|_| AccountSlotEngineError {
                kind: AccountSlotEngineErrorKind::Unavailable,
            })
    }
    async fn complete_deletion(
        &self,
        id: gateway_core::account::AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        self.slots
            .complete_slot_deletion(id)
            .await
            .map_err(|_| AccountSlotEngineError {
                kind: AccountSlotEngineErrorKind::Unavailable,
            })
    }

    async fn list_desired_slots(&self) -> Result<Vec<DesiredAccountSlot>, AccountSlotEngineError> {
        let unavailable = |_| AccountSlotEngineError {
            kind: AccountSlotEngineErrorKind::Unavailable,
        };
        let slots = self.slots.list_account_slots().await.map_err(unavailable)?;
        let accounts = self.accounts.list_accounts().await.map_err(unavailable)?;
        let accounts = accounts
            .into_iter()
            .map(|account| (account.id().clone(), account))
            .collect::<BTreeMap<_, _>>();
        let mut desired = Vec::new();
        for slot in slots.into_iter().filter(|slot| slot.enabled()) {
            let Some(account) = accounts.get(slot.account_id()) else {
                continue;
            };
            if !account.enabled() || account.provider().as_str() != "openai" {
                continue;
            }
            // 槽位缺少代理或账号关闭后停止旧容器；Provider 仍按槽位意图拒绝直连。
            let Some(proxy) = slot.outbound_proxy() else {
                continue;
            };
            let identity = slot.identity();
            desired.push(DesiredAccountSlot {
                account_id: slot.account_id().clone(),
                instance_id: slot.instance_id(),
                generation: slot.generation(),
                hostname: identity.hostname().to_owned(),
                machine_id: identity.machine_id().to_owned(),
                installation_id: identity.installation_id().to_owned(),
                timezone: identity.timezone().to_owned(),
                outbound_proxy: proxy.clone(),
            });
        }
        Ok(desired)
    }
}
