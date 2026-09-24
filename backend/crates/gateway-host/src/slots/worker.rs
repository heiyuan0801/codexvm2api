//! 槽位对账复用 Host 的周期、失败退避和关闭监督。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use gateway_core::task::{
    ScheduledTask, WorkerContribution, WorkerCycleContext, WorkerDefinitionError, WorkerId,
    WorkerKind, WorkerRegistration, WorkerRunnable, WorkerSchedule, WorkerTaskError,
};

use super::{AccountSlotReconciler, AccountSlotRegistry};

pub struct AccountSlotWorker {
    reconciler: AccountSlotReconciler,
    registry: Arc<AccountSlotRegistry>,
}

impl AccountSlotWorker {
    #[must_use]
    pub fn new(reconciler: AccountSlotReconciler, registry: Arc<AccountSlotRegistry>) -> Self {
        Self {
            reconciler,
            registry,
        }
    }

    pub fn into_contribution(
        self,
        interval: Duration,
    ) -> Result<WorkerContribution, WorkerDefinitionError> {
        Ok(WorkerContribution::Registration(
            WorkerRegistration::try_new(
                WorkerId::try_new(WorkerKind::AccountSlotReconciliation, "openai")?,
                WorkerRunnable::Scheduled {
                    schedule: WorkerSchedule::try_new(
                        interval,
                        Duration::from_secs(1),
                        Duration::from_secs(60),
                        Duration::from_secs(60),
                        Duration::from_secs(20),
                    )?,
                    // Docker 状态属于当前单副本网关，不申请跨实例 Redis lease。
                    lease: None,
                    task: Box::new(self),
                },
            )?,
        ))
    }
}

impl ScheduledTask for AccountSlotWorker {
    fn run_cycle(&self, context: WorkerCycleContext) -> BoxFuture<'_, Result<(), WorkerTaskError>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                () = context.cancellation().cancelled() => {
                    self.registry.replace(BTreeMap::new());
                    Ok(())
                }
                result = self.reconciler.reconcile_once() => result.map_err(|_| WorkerTaskError::safe("account slot reconciliation failed")),
            }
        })
    }
}

impl Drop for AccountSlotWorker {
    fn drop(&mut self) {
        self.registry.replace(BTreeMap::new());
    }
}
