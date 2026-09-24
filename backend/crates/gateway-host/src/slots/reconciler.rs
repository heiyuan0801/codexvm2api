//! 账号槽位期望状态到 Docker 实际状态的幂等收敛策略。

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use gateway_core::account::ProviderAccountId;

use super::{
    AccountSlotDesiredStateSource, AccountSlotEngine, AccountSlotEngineError, AccountSlotRegistry,
    AccountSlotRuntimeState,
};

pub struct AccountSlotReconciler {
    enabled: bool,
    source: Arc<dyn AccountSlotDesiredStateSource>,
    engine: Arc<dyn AccountSlotEngine>,
    registry: Arc<AccountSlotRegistry>,
}

impl AccountSlotReconciler {
    #[must_use]
    pub fn new(
        enabled: bool,
        source: Arc<dyn AccountSlotDesiredStateSource>,
        engine: Arc<dyn AccountSlotEngine>,
        registry: Arc<AccountSlotRegistry>,
    ) -> Self {
        Self {
            enabled,
            source,
            engine,
            registry,
        }
    }

    /// 执行一次完整对账。部分槽位失败不会阻止其他账号恢复。
    ///
    /// # Errors
    ///
    /// 无法枚举期望状态或 owned Docker 资源时返回错误；单槽失败以未发布 route 表示。
    pub async fn reconcile_once(&self) -> Result<(), AccountSlotEngineError> {
        // 查询失败、取消或 panic 都不能保留上一轮 Ready 路由。
        let mut publication = RoutePublication {
            registry: &self.registry,
            committed: false,
        };
        let existing = self.engine.list_owned().await?;
        if !self.enabled {
            for slot in existing {
                self.engine.stop(slot.instance_id).await?;
            }
            self.registry.replace(BTreeMap::new());
            return Ok(());
        }

        let desired = self.source.list_desired_slots().await?;
        let deletions = self
            .source
            .pending_deletions()
            .await?
            .into_iter()
            .collect::<BTreeSet<_>>();
        // 删除优先于本轮较早读取的运行快照，避免清理后又按旧快照创建。
        let desired = desired
            .into_iter()
            .filter(|slot| !deletions.contains(&slot.instance_id))
            .collect::<Vec<_>>();
        let desired_instances = desired
            .iter()
            .map(|slot| slot.instance_id)
            .collect::<BTreeSet<_>>();
        // 先撤销不再满足账号/代理条件的路由，再等待 Docker 停止孤儿容器。
        self.registry
            .retain_accounts(&desired.iter().map(|slot| slot.account_id.clone()).collect());
        let mut container_states = BTreeMap::new();
        for slot in existing {
            if deletions.contains(&slot.instance_id) {
                continue;
            }
            if !desired_instances.contains(&slot.instance_id) {
                self.engine.stop(slot.instance_id).await?;
                container_states.insert(slot.instance_id, AccountSlotRuntimeState::Stopped);
            } else {
                container_states.insert(slot.instance_id, slot.state);
            }
        }

        for id in deletions {
            let result = match self.engine.delete(id).await {
                Ok(()) => self.source.complete_deletion(id).await,
                Err(error) => Err(error),
            };
            if result.is_err() {
                // 保留删除意图，下一轮继续清理；单槽失败不影响其他槽位路由。
                container_states.insert(id, AccountSlotRuntimeState::Degraded);
            }
        }

        let mut routes = BTreeMap::<ProviderAccountId, _>::new();
        let mut observations = BTreeMap::new();
        for slot in desired {
            let result = self.engine.converge(&slot).await;
            let health = match &result {
                Ok(converged) => converged.health.clone(),
                Err(_) => super::AccountSlotHealth {
                    instance_id: slot.instance_id,
                    generation: slot.generation,
                    state: AccountSlotRuntimeState::Degraded,
                    reason: Some("slot reconciliation failed"),
                    egress: None,
                },
            };
            container_states.insert(slot.instance_id, health.state);
            observations.insert(slot.account_id.clone(), health);
            match result {
                Ok(converged)
                    if converged.health.state == AccountSlotRuntimeState::Ready
                        && converged.health.instance_id == slot.instance_id
                        && converged.health.generation == slot.generation =>
                {
                    if let Some(route) = converged.route {
                        routes.insert(slot.account_id, route);
                    } else {
                        self.registry.remove(&slot.account_id);
                    }
                }
                Ok(_) | Err(_) => {
                    self.registry.remove(&slot.account_id);
                }
            }
        }
        self.registry.observe_containers(container_states);
        self.registry.publish(routes, observations);
        publication.committed = true;
        Ok(())
    }
}

struct RoutePublication<'a> {
    registry: &'a AccountSlotRegistry,
    committed: bool,
}

impl Drop for RoutePublication<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.registry.replace(BTreeMap::new());
        }
    }
}
