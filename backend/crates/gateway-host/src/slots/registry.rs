//! Provider 与 Admin 共享的槽位观测；路由必须匹配持久化配置代次。

use super::{AccountSlotHealth, AccountSlotRuntimeState};
use gateway_admin::ports::account_slots::AccountSlotControl;
use gateway_core::account::{
    AccountSlotEgress, AccountSlotRoute, AccountSlotRuntime, AccountSlotState, ProviderAccountId,
    ProviderAccountSlot,
};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::RwLock;

pub struct AccountSlotRegistry {
    enabled: bool,
    state: RwLock<RegistryState>,
}

#[derive(Default)]
struct RegistryState {
    routes: BTreeMap<ProviderAccountId, AccountSlotRoute>,
    observations: BTreeMap<ProviderAccountId, AccountSlotHealth>,
    failed: bool,
    containers:
        Option<BTreeMap<gateway_core::account::AccountSlotInstanceId, AccountSlotRuntimeState>>,
}

impl Default for AccountSlotRegistry {
    fn default() -> Self {
        Self::new(true)
    }
}

impl std::fmt::Debug for AccountSlotRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccountSlotRegistry")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl AccountSlotRegistry {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            state: RwLock::new(RegistryState::default()),
        }
    }

    pub(super) fn retain_accounts(&self, accounts: &BTreeSet<ProviderAccountId>) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.routes.retain(|account, _| accounts.contains(account));
        state
            .observations
            .retain(|account, _| accounts.contains(account));
    }

    pub(super) fn remove(&self, account: &ProviderAccountId) {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .routes
            .remove(account);
    }

    pub fn replace(&self, routes: BTreeMap<ProviderAccountId, AccountSlotRoute>) {
        *self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RegistryState {
            routes,
            failed: true,
            ..RegistryState::default()
        };
    }

    pub(super) fn observe_containers(
        &self,
        containers: BTreeMap<gateway_core::account::AccountSlotInstanceId, AccountSlotRuntimeState>,
    ) {
        self.state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .containers = Some(containers);
    }

    pub(super) fn publish(
        &self,
        routes: BTreeMap<ProviderAccountId, AccountSlotRoute>,
        observations: BTreeMap<ProviderAccountId, AccountSlotHealth>,
    ) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.routes = routes;
        state.observations = observations;
        state.failed = false;
    }
}

impl AccountSlotRuntime for AccountSlotRegistry {
    fn route(&self, account_id: &ProviderAccountId) -> Option<AccountSlotRoute> {
        if !self.enabled {
            return None;
        }
        self.state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .routes
            .get(account_id)
            .cloned()
    }

    fn egress_for_slot(&self, slot: &ProviderAccountSlot) -> Option<AccountSlotEgress> {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state
            .observations
            .get(slot.account_id())
            .filter(|health| {
                health.instance_id == slot.instance_id() && health.generation == slot.generation()
            })
            .and_then(|health| health.egress.clone())
    }

    fn route_for_slot(&self, slot: &ProviderAccountSlot) -> Option<AccountSlotRoute> {
        if !self.enabled || !slot.enabled() {
            return None;
        }
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let health = state.observations.get(slot.account_id())?;
        if health.instance_id != slot.instance_id()
            || health.generation != slot.generation()
            || health.state != AccountSlotRuntimeState::Ready
        {
            return None;
        }
        state.routes.get(slot.account_id()).cloned()
    }

    fn status(&self, slot: &ProviderAccountSlot) -> AccountSlotState {
        if !slot.enabled() {
            return AccountSlotState::Disabled;
        }
        if !self.enabled {
            return AccountSlotState::GlobalDisabled;
        }
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.failed {
            return AccountSlotState::Degraded;
        }
        let Some(health) = state.observations.get(slot.account_id()).filter(|health| {
            health.instance_id == slot.instance_id() && health.generation == slot.generation()
        }) else {
            return AccountSlotState::Starting;
        };
        if health.state == AccountSlotRuntimeState::Ready
            && state.routes.contains_key(slot.account_id())
        {
            AccountSlotState::Ready
        } else {
            AccountSlotState::Degraded
        }
    }
}

impl AccountSlotControl for AccountSlotRegistry {
    fn container_state(&self, id: gateway_core::account::AccountSlotInstanceId) -> &'static str {
        let state = self
            .state
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(containers) = &state.containers else {
            return "unknown";
        };
        match containers.get(&id) {
            None => "not-created",
            Some(AccountSlotRuntimeState::Stopped) => "stopped",
            Some(AccountSlotRuntimeState::Degraded) => "degraded",
            Some(_) => "stopping",
        }
    }
    fn globally_enabled(&self) -> bool {
        self.enabled
    }
    fn invalidate(&self, account: &ProviderAccountId) {
        let mut state = self
            .state
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.routes.remove(account);
        state.observations.remove(account);
    }
}
