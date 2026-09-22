//! Provider 可读取、监督器可原子替换的 Ready 槽位注册表。

use std::collections::BTreeMap;
use std::sync::RwLock;

use gateway_core::account::{AccountSlotRoute, AccountSlotRuntime, ProviderAccountId};

#[derive(Debug, Default)]
pub struct AccountSlotRegistry {
    routes: RwLock<BTreeMap<ProviderAccountId, AccountSlotRoute>>,
}

impl AccountSlotRegistry {
    pub fn replace(&self, routes: BTreeMap<ProviderAccountId, AccountSlotRoute>) {
        *self
            .routes
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = routes;
    }
}

impl AccountSlotRuntime for AccountSlotRegistry {
    fn route(&self, account_id: &ProviderAccountId) -> Option<AccountSlotRoute> {
        self.routes
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(account_id)
            .cloned()
    }
}
