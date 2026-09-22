use super::*;
use gateway_core::lifecycle::CancellationToken;
use gateway_core::task::{ScheduledTask, WorkerCycleContext, WorkerId, WorkerKind};
use gateway_host::slots::{AccountSlotEngineErrorKind, AccountSlotWorker};
use std::future::pending;
use std::time::Duration;

struct FailedSource;

#[async_trait]
impl AccountSlotDesiredStateSource for FailedSource {
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
        Err(AccountSlotEngineError {
            kind: AccountSlotEngineErrorKind::Unavailable,
        })
    }
}

struct PendingSource(tokio::sync::Notify);

#[async_trait]
impl AccountSlotDesiredStateSource for PendingSource {
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
        self.0.notify_one();
        pending().await
    }
}

fn stale_registry() -> Arc<AccountSlotRegistry> {
    let registry = Arc::new(AccountSlotRegistry::default());
    registry.replace(BTreeMap::from([(
        ProviderAccountId::new("acct_stale").unwrap(),
        route(),
    )]));
    registry
}

fn assert_empty(registry: &AccountSlotRegistry) {
    assert!(
        registry
            .route(&ProviderAccountId::new("acct_stale").unwrap())
            .is_none()
    );
}

#[tokio::test]
async fn store_failure_revokes_previous_ready_routes() {
    let registry = stale_registry();
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let reconciler = AccountSlotReconciler::new(
        true,
        Arc::new(FailedSource),
        Arc::new(FakeEngine::new(id, id)),
        registry.clone(),
    );
    assert!(reconciler.reconcile_once().await.is_err());
    assert_empty(&registry);
}

#[tokio::test]
async fn cancellation_interrupts_pending_reconcile_and_revokes_routes() {
    let registry = stale_registry();
    let source = Arc::new(PendingSource(tokio::sync::Notify::new()));
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let reconciler = AccountSlotReconciler::new(
        true,
        source.clone(),
        Arc::new(FakeEngine::new(id, id)),
        registry.clone(),
    );
    let worker = Arc::new(AccountSlotWorker::new(reconciler, registry.clone()));
    let cancellation = CancellationToken::new();
    let context = WorkerCycleContext::new(
        WorkerId::try_new(WorkerKind::AccountSlotReconciliation, "test").unwrap(),
        None,
        cancellation.clone(),
    );
    let task_worker = worker.clone();
    let task = tokio::spawn(async move { task_worker.run_cycle(context).await });
    source.0.notified().await;
    cancellation.cancel();
    tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_empty(&registry);
}

#[tokio::test]
async fn aborting_reconcile_revokes_routes_even_without_cooperative_cancel() {
    let registry = stale_registry();
    let source = Arc::new(PendingSource(tokio::sync::Notify::new()));
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let reconciler = AccountSlotReconciler::new(
        true,
        source.clone(),
        Arc::new(FakeEngine::new(id, id)),
        registry.clone(),
    );
    let task = tokio::spawn(async move { reconciler.reconcile_once().await });
    source.0.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_empty(&registry);
}

#[test]
fn dropping_idle_worker_revokes_routes() {
    let registry = stale_registry();
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let reconciler = AccountSlotReconciler::new(
        true,
        Arc::new(FakeSource(vec![])),
        Arc::new(FakeEngine::new(id, id)),
        registry.clone(),
    );
    drop(AccountSlotWorker::new(reconciler, registry.clone()));
    assert_empty(&registry);
}

#[tokio::test]
async fn docker_failure_revokes_previous_ready_routes() {
    let registry = stale_registry();
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let mut engine = FakeEngine::new(id, id);
    engine.fail_list = true;
    let reconciler = AccountSlotReconciler::new(
        true,
        Arc::new(FakeSource(vec![])),
        Arc::new(engine),
        registry.clone(),
    );
    assert!(reconciler.reconcile_once().await.is_err());
    assert_empty(&registry);
}

#[tokio::test]
async fn contribution_runs_local_periodic_reconciliation() {
    use gateway_core::task::{WorkerContribution, WorkerRunnable};
    let ready = desired("acct_ready", "0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
    let other = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let registry = Arc::new(AccountSlotRegistry::default());
    let reconciler = AccountSlotReconciler::new(
        true,
        Arc::new(FakeSource(vec![ready.clone()])),
        Arc::new(FakeEngine::new(other, other)),
        registry.clone(),
    );
    let WorkerContribution::Registration(registration) =
        AccountSlotWorker::new(reconciler, registry.clone())
            .into_contribution(Duration::from_secs(7))
            .unwrap()
    else {
        panic!("active worker expected")
    };
    let WorkerRunnable::Scheduled {
        schedule,
        lease,
        task,
    } = registration.runnable
    else {
        panic!("scheduled worker expected")
    };
    assert_eq!(schedule.interval(), Duration::from_secs(7));
    assert!(lease.is_none());
    task.run_cycle(WorkerCycleContext::new(
        registration.id,
        None,
        CancellationToken::new(),
    ))
    .await
    .unwrap();
    assert!(registry.route(&ready.account_id).is_some());
    drop(task);
    assert!(registry.route(&ready.account_id).is_none());
}

#[tokio::test]
async fn orphan_route_is_revoked_before_slow_container_stop() {
    let registry = stale_registry();
    let id = instance("0199f4c8-52a8-7aa0-a6d7-f75219e82e40");
    let started = Arc::new(tokio::sync::Notify::new());
    let mut engine = FakeEngine::new(id, id);
    engine.stop_started = Some(started.clone());
    let reconciler = AccountSlotReconciler::new(
        true,
        Arc::new(FakeSource(vec![])),
        Arc::new(engine),
        registry.clone(),
    );
    let task = tokio::spawn(async move { reconciler.reconcile_once().await });
    started.notified().await;
    assert_empty(&registry);
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
}
