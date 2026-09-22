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
    async fn list_desired_slots(&self) -> Result<Vec<DesiredAccountSlot>, AccountSlotEngineError> {
        Ok(self.0.clone())
    }
}

struct FakeEngine {
    existing: AccountSlotInstanceId,
    degraded: AccountSlotInstanceId,
    stopped: Mutex<Vec<AccountSlotInstanceId>>,
}

impl FakeEngine {
    fn new(existing: AccountSlotInstanceId, degraded: AccountSlotInstanceId) -> Self {
        Self {
            existing,
            degraded,
            stopped: Mutex::new(Vec::new()),
        }
    }
}

#[async_trait]
impl AccountSlotEngine for FakeEngine {
    async fn list_owned(&self) -> Result<Vec<AccountSlotHealth>, AccountSlotEngineError> {
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
