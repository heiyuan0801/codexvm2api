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
        let existing = self.engine.list_owned().await?;
        if !self.enabled {
            for slot in existing {
                self.engine.stop(slot.instance_id).await?;
            }
            self.registry.replace(BTreeMap::new());
            return Ok(());
        }

        let desired = self.source.list_desired_slots().await?;
        let desired_instances = desired
            .iter()
            .map(|slot| slot.instance_id)
            .collect::<BTreeSet<_>>();
        for slot in existing {
            if !desired_instances.contains(&slot.instance_id) {
                self.engine.stop(slot.instance_id).await?;
            }
        }

        let mut routes = BTreeMap::<ProviderAccountId, _>::new();
        for slot in desired {
            match self.engine.converge(&slot).await {
                Ok(converged) if converged.health.state == AccountSlotRuntimeState::Ready => {
                    if let Some(route) = converged.route {
                        routes.insert(slot.account_id, route);
                    }
                }
                Ok(_) | Err(_) => {}
            }
        }
        self.registry.replace(routes);
        Ok(())
    }
}
