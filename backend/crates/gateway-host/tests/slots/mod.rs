#[cfg(unix)]
mod docker;
mod egress;
mod live;
mod source;
mod worker;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use gateway_core::account::{
    AccountSlotBearerToken, AccountSlotGeneration, AccountSlotInstanceId, AccountSlotRoute,
    AccountSlotRuntime, OutboundProxy, ProviderAccountId,
};
use gateway_host::slots::{
    AccountSlotDesiredStateSource, AccountSlotEngine, AccountSlotEngineError, AccountSlotHealth,
    AccountSlotReconciler, AccountSlotRegistry, AccountSlotRuntimeState, ConvergedAccountSlot,
    DesiredAccountSlot,
};

#[tokio::test]
async fn reconcile_publishes_only_ready_slots_and_stops_orphans() {
    let ready = desired("acct_ready", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    let degraded = desired("acct_degraded", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3e");
    let orphan = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e3f");
    let source = Arc::new(FakeSource(vec![ready.clone(), degraded.clone()]));
    let engine = Arc::new(FakeEngine::new(orphan, degraded.instance_id));
    let registry = Arc::new(AccountSlotRegistry::default());
    let reconciler =
        AccountSlotReconciler::new(true, source, engine.clone(), Arc::clone(&registry));

    reconciler.reconcile_once().await.expect("reconcile");

    assert!(registry.route(&ready.account_id).is_some());
    assert!(registry.route(&degraded.account_id).is_none());
    assert_eq!(*engine.stopped.lock().unwrap(), vec![orphan]);
}

#[tokio::test]
async fn global_disable_stops_all_owned_slots_and_clears_routes() {
    let existing = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let source = Arc::new(FakeSource(Vec::new()));
    let engine = Arc::new(FakeEngine::new(existing, existing));
    let registry = Arc::new(AccountSlotRegistry::default());
    registry.replace(BTreeMap::from([(
        ProviderAccountId::new("acct_stale").unwrap(),
        route(),
    )]));
    let reconciler =
        AccountSlotReconciler::new(false, source, engine.clone(), Arc::clone(&registry));

    reconciler.reconcile_once().await.expect("reconcile");

    assert!(
        registry
            .route(&ProviderAccountId::new("acct_stale").unwrap())
            .is_none()
    );
    assert_eq!(*engine.stopped.lock().unwrap(), vec![existing]);
}

fn desired(account: &str, id: &str) -> DesiredAccountSlot {
    DesiredAccountSlot {
        account_id: ProviderAccountId::new(account).unwrap(),
        instance_id: instance(id),
        generation: AccountSlotGeneration::new(1).unwrap(),
        hostname: format!("cpr-slot-{}", &id[..8]),
        machine_id: "0123456789abcdef0123456789abcdef".to_owned(),
        installation_id: "0199f4c8-52a8-7aa0-a6d7-f75219e82e41".to_owned(),
        timezone: "Asia/Shanghai".to_owned(),
        outbound_proxy: OutboundProxy::parse("socks5://user:pass@127.0.0.1:1080").unwrap(),
    }
}

fn instance(value: &str) -> AccountSlotInstanceId {
    AccountSlotInstanceId::parse(&format!("slot_{value}")).unwrap()
}

fn route() -> AccountSlotRoute {
    AccountSlotRoute::new(
        reqwest::Url::parse("http://slot.internal:8090/internal/v1/forward").unwrap(),
        AccountSlotBearerToken::new(vec![b'x'; 32]).unwrap(),
    )
}

struct FakeSource(Vec<DesiredAccountSlot>);

#[async_trait]
impl AccountSlotDesiredStateSource for FakeSource {
    async fn pending_deletions(
        &self,
    ) -> Result<Vec<AccountSlotInstanceId>, AccountSlotEngineError> {
        Ok(vec![])
    }
    async fn complete_deletion(
        &self,
        _id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        unreachable!("no pending deletions")
    }

    async fn list_desired_slots(&self) -> Result<Vec<DesiredAccountSlot>, AccountSlotEngineError> {
        Ok(self.0.clone())
    }
}

struct FakeEngine {
    existing: AccountSlotInstanceId,
    degraded: AccountSlotInstanceId,
    stopped: Mutex<Vec<AccountSlotInstanceId>>,
    fail_list: bool,
    deleted: Mutex<Vec<AccountSlotInstanceId>>,
    fail_delete: std::sync::atomic::AtomicBool,
    stop_started: Option<Arc<tokio::sync::Notify>>,
}

impl FakeEngine {
    fn new(existing: AccountSlotInstanceId, degraded: AccountSlotInstanceId) -> Self {
        Self {
            existing,
            degraded,
            stopped: Mutex::new(Vec::new()),
            fail_list: false,
            deleted: Mutex::new(vec![]),
            fail_delete: std::sync::atomic::AtomicBool::new(false),
            stop_started: None,
        }
    }
}

#[async_trait]
impl AccountSlotEngine for FakeEngine {
    async fn delete(&self, id: AccountSlotInstanceId) -> Result<(), AccountSlotEngineError> {
        if self.fail_delete.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(AccountSlotEngineError {
                kind: gateway_host::slots::AccountSlotEngineErrorKind::Unavailable,
            });
        }
        self.deleted.lock().unwrap().push(id);
        Ok(())
    }

    async fn list_owned(&self) -> Result<Vec<AccountSlotHealth>, AccountSlotEngineError> {
        if self.fail_list {
            return Err(AccountSlotEngineError {
                kind: gateway_host::slots::AccountSlotEngineErrorKind::Unavailable,
            });
        }
        Ok(vec![health(
            self.existing,
            "acct_existing",
            AccountSlotRuntimeState::Ready,
        )])
    }

    async fn inspect(
        &self,
        instance_id: AccountSlotInstanceId,
    ) -> Result<Option<AccountSlotHealth>, AccountSlotEngineError> {
        Ok(Some(health(
            instance_id,
            "acct_inspected",
            AccountSlotRuntimeState::Ready,
        )))
    }

    async fn converge(
        &self,
        desired: &DesiredAccountSlot,
    ) -> Result<ConvergedAccountSlot, AccountSlotEngineError> {
        let state = if desired.instance_id == self.degraded {
            AccountSlotRuntimeState::Degraded
        } else {
            AccountSlotRuntimeState::Ready
        };
        Ok(ConvergedAccountSlot {
            health: AccountSlotHealth {
                instance_id: desired.instance_id,
                generation: desired.generation,
                state,
                reason: None,
            },
            route: (state == AccountSlotRuntimeState::Ready).then(route),
        })
    }

    async fn stop(&self, instance_id: AccountSlotInstanceId) -> Result<(), AccountSlotEngineError> {
        self.stopped.lock().unwrap().push(instance_id);
        if let Some(started) = &self.stop_started {
            started.notify_one();
            std::future::pending::<()>().await;
        }
        Ok(())
    }
}

fn health(
    instance_id: AccountSlotInstanceId,
    _account_id: &str,
    state: AccountSlotRuntimeState,
) -> AccountSlotHealth {
    AccountSlotHealth {
        instance_id,
        generation: AccountSlotGeneration::new(1).unwrap(),
        state,
        reason: None,
    }
}

#[tokio::test]
async fn registry_rejects_old_generation_and_invalidated_routes() {
    use gateway_admin::ports::account_slots::AccountSlotControl;
    use gateway_core::account::{AccountSlotIdentity, AccountSlotState, ProviderAccountSlot};
    let desired = desired("acct_generation", "0199f4c8-52a8-7aa0-a6d7-f75219e82e55");
    let engine = Arc::new(FakeEngine::new(
        instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e56"),
        instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e57"),
    ));
    let registry = Arc::new(AccountSlotRegistry::default());
    let slot = |generation| {
        ProviderAccountSlot::new(
            desired.account_id.clone(),
            true,
            desired.instance_id,
            AccountSlotIdentity::new(
                desired.hostname.clone(),
                desired.machine_id.clone(),
                desired.installation_id.clone(),
                desired.timezone.clone(),
            )
            .unwrap(),
            AccountSlotGeneration::new(generation).unwrap(),
        )
    };
    AccountSlotReconciler::new(
        true,
        Arc::new(FakeSource(vec![desired.clone()])),
        engine,
        registry.clone(),
    )
    .reconcile_once()
    .await
    .unwrap();
    assert!(registry.route_for_slot(&slot(1)).is_some());
    assert_eq!(registry.status(&slot(1)), AccountSlotState::Ready);
    assert!(registry.route_for_slot(&slot(2)).is_none());
    assert_eq!(registry.status(&slot(2)), AccountSlotState::Starting);
    registry.invalidate(&desired.account_id);
    assert!(registry.route_for_slot(&slot(1)).is_none());
    assert_eq!(registry.status(&slot(1)), AccountSlotState::Starting);
    assert_eq!(
        AccountSlotRegistry::new(false).status(&slot(1)),
        AccountSlotState::GlobalDisabled
    );
    registry.replace(BTreeMap::new());
    assert_eq!(registry.status(&slot(1)), AccountSlotState::Degraded);
}

struct DeletingSource {
    desired: DesiredAccountSlot,
    pending: Mutex<Vec<AccountSlotInstanceId>>,
}
#[async_trait]
impl AccountSlotDesiredStateSource for DeletingSource {
    async fn list_desired_slots(&self) -> Result<Vec<DesiredAccountSlot>, AccountSlotEngineError> {
        Ok(vec![self.desired.clone()])
    }
    async fn pending_deletions(
        &self,
    ) -> Result<Vec<AccountSlotInstanceId>, AccountSlotEngineError> {
        Ok(self.pending.lock().unwrap().clone())
    }
    async fn complete_deletion(
        &self,
        id: AccountSlotInstanceId,
    ) -> Result<(), AccountSlotEngineError> {
        self.pending
            .lock()
            .unwrap()
            .retain(|pending| *pending != id);
        Ok(())
    }
}

#[tokio::test]
async fn deletion_failure_retains_intent_retries_and_does_not_block_other_routes() {
    use gateway_admin::ports::account_slots::AccountSlotControl;
    use std::sync::atomic::Ordering;
    let ready = desired("acct_ready", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    let deleting = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e3f");
    let source = Arc::new(DeletingSource {
        desired: ready.clone(),
        pending: Mutex::new(vec![deleting]),
    });
    let engine = Arc::new(FakeEngine::new(deleting, deleting));
    engine.fail_delete.store(true, Ordering::SeqCst);
    let registry = Arc::new(AccountSlotRegistry::default());
    let reconciler =
        AccountSlotReconciler::new(true, source.clone(), engine.clone(), registry.clone());
    reconciler.reconcile_once().await.unwrap();
    assert_eq!(*source.pending.lock().unwrap(), vec![deleting]);
    assert_eq!(registry.container_state(deleting), "degraded");
    assert!(registry.route(&ready.account_id).is_some());
    engine.fail_delete.store(false, Ordering::SeqCst);
    reconciler.reconcile_once().await.unwrap();
    assert!(source.pending.lock().unwrap().is_empty());
    assert_eq!(*engine.deleted.lock().unwrap(), vec![deleting]);
    assert!(registry.route(&ready.account_id).is_some());
}

#[tokio::test]
async fn deletion_overrides_stale_running_snapshot() {
    let stale = desired("acct_stale", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    let id = stale.instance_id;
    let source = Arc::new(DeletingSource {
        desired: stale.clone(),
        pending: Mutex::new(vec![id]),
    });
    let engine = Arc::new(FakeEngine::new(id, id));
    let registry = Arc::new(AccountSlotRegistry::default());
    AccountSlotReconciler::new(true, source, engine.clone(), registry.clone())
        .reconcile_once()
        .await
        .unwrap();
    assert_eq!(*engine.deleted.lock().unwrap(), vec![id]);
    assert!(registry.route(&stale.account_id).is_none());
}
