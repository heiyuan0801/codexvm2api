//! 槽位依赖的惰性组装；全局关闭时不触碰 Docker 或容器环境。

use std::sync::Arc;
use std::time::Duration;

use gateway_core::account::{ProviderAccountSlotStore, ProviderAccountStore};
use gateway_core::task::{WorkerContribution, WorkerDisabledReason, WorkerKind};

use super::{
    AccountSlotEngineError, AccountSlotEngineErrorKind, AccountSlotReconciler, AccountSlotRegistry,
    AccountSlotWorker, BollardAccountSlotEngine, StoredAccountSlotDesiredStateSource,
};
use crate::config::OpenAiSlotsConfig;

pub struct AccountSlotsBundle {
    pub registry: Arc<AccountSlotRegistry>,
    pub worker: WorkerContribution,
}

pub fn initialize_account_slots(
    config: OpenAiSlotsConfig,
    slots: Arc<dyn ProviderAccountSlotStore>,
    accounts: Arc<dyn ProviderAccountStore>,
) -> Result<AccountSlotsBundle, AccountSlotEngineError> {
    let registry = Arc::new(AccountSlotRegistry::new(config.enabled));
    if !config.enabled {
        return Ok(AccountSlotsBundle {
            registry,
            worker: WorkerContribution::Disabled {
                kind: WorkerKind::AccountSlotReconciliation,
                reason: WorkerDisabledReason::AccountSlotsDisabled,
            },
        });
    }
    let invalid = || AccountSlotEngineError {
        kind: AccountSlotEngineErrorKind::InvalidState,
    };
    let container = config
        .gateway_container
        .clone()
        .or_else(|| std::env::var("HOSTNAME").ok())
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(invalid)?;
    let interval = Duration::from_secs(config.reconcile_interval_seconds);
    let engine = Arc::new(BollardAccountSlotEngine::connect(config, container)?);
    let source = Arc::new(StoredAccountSlotDesiredStateSource::new(slots, accounts));
    let reconciler = AccountSlotReconciler::new(true, source, engine, registry.clone());
    let worker = AccountSlotWorker::new(reconciler, registry.clone())
        .into_contribution(interval)
        .map_err(|_| invalid())?;
    Ok(AccountSlotsBundle { registry, worker })
}
