//! 每个槽位一套的转发入口与 `iptables` 规则的生命周期。
//!
//! 对账每次都会调用 [`SlotEgressManager::ensure`]，因此这里必须是幂等的：
//! 入口端口由内核分配并记在句柄里，重复调用只更新出口代理配置；
//! 规则由 [`SlotEgressManager::apply_rules`] 重放，重复调用不会累积跳转。
//!
//! 句柄按实例 ID 保存，跨对账周期存活：槽位容器的重启策略会在容器退出后自动拉起，
//! 若入口随某次对账结束而关闭，容器重启后就再也没有出网路径。

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;

use gateway_core::account::{AccountSlotInstanceId, OutboundProxy};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use super::iptables::{Iptables, forward_parent};
use super::plan::{EgressTarget, RedirectPorts, plan};
use super::transport::{SlotEgress, SlotEgressConfig};
use super::{EgressError, net};

/// 一个正在运行的槽位入口。
struct RunningEgress {
    /// 入口实际占用的端口；也是 `iptables` 计划里写死的重定向目标。
    ports: RedirectPorts,
    /// 配置推送端；对账更新出口代理时只发一条消息，不重建入口。
    config: watch::Sender<SlotEgressConfig>,
    /// 取消信号；置 `true` 让三个监听任务退出。
    cancelled: watch::Sender<bool>,
    /// 三个监听任务；只在取消失败时兜底 abort。
    tasks: JoinHandle<()>,
}

/// 槽位出网入口的注册表。
pub struct SlotEgressManager {
    iptables: Arc<dyn Iptables>,
    running: HashMap<AccountSlotInstanceId, RunningEgress>,
}

impl std::fmt::Debug for SlotEgressManager {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SlotEgressManager")
            .field("slots", &self.running.len())
            .finish_non_exhaustive()
    }
}

impl SlotEgressManager {
    /// 构造入口注册表；`iptables` 由调用方注入，便于在无内核网络栈的平台上替换。
    #[must_use]
    pub fn new(iptables: Arc<dyn Iptables>) -> Self {
        Self {
            iptables,
            running: HashMap::new(),
        }
    }

    /// 该槽位的规则目标；子网与网桥名都由实例 ID 推导，容器重建后保持不变。
    #[must_use]
    pub fn target(&self, instance: AccountSlotInstanceId) -> EgressTarget {
        EgressTarget {
            bridge: net::bridge_name(instance),
            subnet: net::subnet_for(instance),
            // 端口在入口绑定后才确定，规则重放时由 `apply_rules` 覆盖。
            ports: RedirectPorts {
                dns: 0,
                tls: 0,
                http: 0,
            },
        }
    }

    /// 确保槽位入口在运行，并把出口代理配置推给正在服务的连接。
    ///
    /// # Errors
    ///
    /// 子网不可推导、网关地址不属于本机或端口被占用时返回错误；
    /// 调用方必须因此中止该槽位的收敛，不能启动一个没有出网约束的容器。
    pub async fn ensure(
        &mut self,
        instance: AccountSlotInstanceId,
        proxy: &OutboundProxy,
    ) -> Result<(), EgressError> {
        let config = SlotEgressConfig {
            proxy: Some(proxy.clone()),
        };
        if let Some(running) = self.running.get(&instance) {
            let _ = running.config.send(config);
            return Ok(());
        }
        let subnet = self.target(instance).subnet;
        let gateway = net::gateway_of(&subnet).ok_or(EgressError::Network)?;
        // 先用 0 让内核挑选端口：宿主上其它进程占用的端口不需要对账自己避开。
        let egress = SlotEgress::bind(IpAddr::V4(gateway), RedirectPorts::EPHEMERAL)
            .await
            .map_err(|_| EgressError::Ports)?;
        let ports = egress.ports().map_err(|_| EgressError::Ports)?;
        let (config_tx, config_rx) = watch::channel(config);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let tasks = tokio::spawn(egress.serve(config_rx, cancel_rx));
        self.running.insert(
            instance,
            RunningEgress {
                ports,
                config: config_tx,
                cancelled: cancel_tx,
                tasks,
            },
        );
        Ok(())
    }

    /// 下发该槽位的全部 `iptables` 规则。
    ///
    /// 与 [`SlotEgressManager::ensure`] 分开：规则里的重定向端口来自入口句柄，
    /// 只能在内核已分配端口之后生成。
    ///
    /// # Errors
    ///
    /// 入口未运行或 `iptables` 拒绝规则时返回错误。
    pub async fn apply_rules(&self, instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        let running = self
            .running
            .get(&instance)
            .ok_or(EgressError::Ports)?;
        let mut target = self.target(instance);
        target.ports = running.ports;
        let chain = target.chain(instance);
        let parent = forward_parent(self.iptables.as_ref()).await?;
        for rule in plan(&target, &chain, parent).apply {
            self.iptables.run(&rule.0).await?;
        }
        Ok(())
    }

    /// 关闭入口并移除该槽位的全部规则。
    ///
    /// 规则必须先于网络拆除：网桥不存在时 `-i <bridge>` 规则会因缺少接口而删除失败，
    /// 留下悬空跳转。
    ///
    /// # Errors
    ///
    /// `iptables` 拒绝拆除时返回错误，删除意图得以保留以便重试。
    pub async fn remove(&mut self, instance: AccountSlotInstanceId) -> Result<(), EgressError> {
        let Some(running) = self.running.remove(&instance) else {
            return Ok(());
        };
        // 先摘规则再停入口：入口还活着时端口不会被别的进程抢走，
        // 规则拆除期间到达的连接仍会被安全地转发或丢弃，而不是落到直连路径上。
        let mut target = self.target(instance);
        target.ports = running.ports;
        let chain = target.chain(instance);
        let parent = forward_parent(self.iptables.as_ref()).await?;
        for rule in plan(&target, &chain, parent).teardown {
            self.iptables.run(&rule.0).await?;
        }
        let _ = running.cancelled.send(true);
        let _ = running.tasks.await;
        Ok(())
    }
}

impl Drop for SlotEgressManager {
    fn drop(&mut self) {
        // 进程退出时监听器随进程消失，但留在内核里的跳转不会；
        // 取消在跑的入口，避免它继续服务已经无人管理的规则。
        for (_, running) in self.running.drain() {
            let _ = running.cancelled.send(true);
            running.tasks.abort();
        }
    }
}
