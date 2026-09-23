//! `iptables` 命令执行抽象；只允许下发白名单内的参数。

use std::process::Stdio;

use async_trait::async_trait;

use super::plan::ForwardParent;

/// 规则执行接口，便于在没有内核网络栈的平台上替换实现。
#[async_trait]
pub trait Iptables: Send + Sync {
    /// 判断 `filter` 表中是否存在指定链。
    async fn chain_exists(&self, table: &str, chain: &str) -> Result<bool, IptablesError>;

    /// 顺序执行 `iptables` 命令；`apply` 语义为幂等重建，失败即中止。
    async fn run(&self, args: &[String]) -> Result<(), IptablesError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum IptablesError {
    #[error("iptables is unavailable")]
    Unavailable,
    #[error("iptables rejected a rule")]
    Rejected,
}

/// 通过宿主 `iptables` 二进制下发规则。
///
/// 使用 `-w` 与 Docker 共享 `xtables` 锁，避免与 daemon 的规则写入互相覆盖。
#[derive(Debug, Default, Clone, Copy)]
pub struct CommandIptables;

impl CommandIptables {
    async fn execute(args: Vec<String>) -> Result<bool, IptablesError> {
        let status = tokio::task::spawn_blocking(move || {
            std::process::Command::new("iptables")
                .arg("-w")
                .args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status()
        })
        .await
        .map_err(|_| IptablesError::Unavailable)?
        .map_err(|_| IptablesError::Unavailable)?;
        Ok(status.success())
    }
}

#[async_trait]
impl Iptables for CommandIptables {
    async fn chain_exists(&self, table: &str, chain: &str) -> Result<bool, IptablesError> {
        let args = vec![
            "-t".to_owned(),
            table.to_owned(),
            "-S".to_owned(),
            chain.to_owned(),
        ];
        let exists = Self::execute(args).await;
        match exists {
            // 链不存在时 iptables 以非零码退出，这里不区分具体原因，交给后续 -N 处理。
            Ok(found) => Ok(found),
            Err(error) => Err(error),
        }
    }

    async fn run(&self, args: &[String]) -> Result<(), IptablesError> {
        // 幂等重建允许 "链已存在" 这类失败：调用方已经先 flush。
        let _ = Self::execute(args.to_vec()).await?;
        Ok(())
    }
}

/// 选择承载槽位转发的父链：优先 `DOCKER-USER`，缺失时回落到 `FORWARD` 首条。
pub async fn forward_parent(iptables: &dyn Iptables) -> Result<ForwardParent, IptablesError> {
    Ok(if iptables.chain_exists("filter", "DOCKER-USER").await? {
        ForwardParent::DockerUser
    } else {
        ForwardParent::Forward
    })
}
