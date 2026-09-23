//! `iptables` 命令执行抽象；只允许下发白名单内的参数。

use std::collections::BTreeSet;
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

    /// 清理本进程上次退出后遗留的槽位用户链。
    ///
    /// 非命令实现默认不做清理，测试和受限平台可以显式注入自己的生命周期控制器。
    async fn cleanup_stale_chains(&self) -> Result<usize, IptablesError> {
        Ok(0)
    }
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
    async fn execute_output(args: Vec<String>) -> Result<std::process::Output, IptablesError> {
        tokio::task::spawn_blocking(move || {
            std::process::Command::new("iptables")
                .arg("-w")
                .args(args)
                .stdin(Stdio::null())
                .output()
        })
        .await
        .map_err(|_| IptablesError::Unavailable)?
        .map_err(|_| IptablesError::Unavailable)
    }

    async fn run_checked(args: Vec<String>) -> Result<(), IptablesError> {
        let output = Self::execute_output(args).await?;
        if output.status.success() {
            Ok(())
        } else {
            Err(IptablesError::Rejected)
        }
    }

    async fn list_rules(table: &str) -> Result<Vec<Vec<String>>, IptablesError> {
        let output =
            Self::execute_output(vec!["-t".to_owned(), table.to_owned(), "-S".to_owned()]).await?;
        if !output.status.success() {
            return Err(IptablesError::Rejected);
        }
        let text = std::str::from_utf8(&output.stdout).map_err(|_| IptablesError::Rejected)?;
        Ok(text
            .lines()
            .filter_map(|line| {
                let rule = line
                    .split_whitespace()
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                (!rule.is_empty()).then_some(rule)
            })
            .collect())
    }

    async fn cleanup_table(table: &str) -> Result<usize, IptablesError> {
        let rules = Self::list_rules(table).await?;
        let chains = rules
            .iter()
            .filter_map(|rule| {
                if rule.first().map(String::as_str) != Some("-N") {
                    return None;
                }
                rule.get(1).cloned()
            })
            .filter(|chain| is_managed_chain(chain))
            .collect::<BTreeSet<_>>();
        if chains.is_empty() {
            return Ok(0);
        }

        // 先移除所有指向槽位链的跳转，之后才能 flush/delete 用户链。
        for rule in rules.iter().filter(|rule| {
            rule.first().is_some_and(|first| first == "-A")
                && rule
                    .windows(2)
                    .any(|window| window[0] == "-j" && chains.contains(&window[1]))
        }) {
            let mut delete = rule.clone();
            delete[0] = "-D".to_owned();
            let mut args = vec!["-t".to_owned(), table.to_owned()];
            args.extend(delete);
            Self::run_checked(args).await?;
        }

        for chain in &chains {
            Self::run_checked(vec![
                "-t".to_owned(),
                table.to_owned(),
                "-F".to_owned(),
                chain.clone(),
            ])
            .await?;
            Self::run_checked(vec![
                "-t".to_owned(),
                table.to_owned(),
                "-X".to_owned(),
                chain.clone(),
            ])
            .await?;
        }
        Ok(chains.len())
    }
}

fn is_managed_chain(chain: &str) -> bool {
    let Some(suffix) = chain.strip_prefix("CPR-") else {
        return false;
    };
    suffix.len() == 12 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn stderr_contains(stderr: &[u8], fragments: &[&str]) -> bool {
    let text = String::from_utf8_lossy(stderr).to_ascii_lowercase();
    fragments.iter().all(|fragment| text.contains(fragment))
}

fn is_missing_chain_error(stderr: &[u8]) -> bool {
    stderr_contains(stderr, &["chain"])
        && (["not found", "does not exist", "no chain", "unknown chain"])
            .iter()
            .any(|fragment| stderr_contains(stderr, &[fragment]))
}

fn is_chain_create(args: &[String]) -> bool {
    args.windows(2)
        .any(|window| window[0] == "-N" && !window[1].is_empty())
}

fn is_existing_chain_error(args: &[String], stderr: &[u8]) -> bool {
    is_chain_create(args)
        && stderr_contains(stderr, &["chain"])
        && (["already exists", "file exists", "exists"])
            .iter()
            .any(|fragment| stderr_contains(stderr, &[fragment]))
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
        let output = Self::execute_output(args).await?;
        if output.status.success() {
            return Ok(true);
        }
        // 只有明确的“链不存在”才回退到 FORWARD；权限、锁或表不可用必须中止，
        // 否则网关会把一个没有任何出网规则的槽位误判成可收敛。
        if is_missing_chain_error(&output.stderr) {
            Ok(false)
        } else {
            Err(IptablesError::Rejected)
        }
    }

    async fn run(&self, args: &[String]) -> Result<(), IptablesError> {
        let output = Self::execute_output(args.to_vec()).await?;
        if output.status.success() || is_existing_chain_error(args, &output.stderr) {
            // 幂等重建允许 "链已存在" 这类失败：调用方随后会 flush。
            return Ok(());
        }
        Err(IptablesError::Rejected)
    }

    async fn cleanup_stale_chains(&self) -> Result<usize, IptablesError> {
        let nat = Self::cleanup_table("nat").await?;
        let filter = Self::cleanup_table("filter").await?;
        Ok(nat + filter)
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

#[cfg(test)]
mod tests {
    use super::{
        is_chain_create, is_existing_chain_error, is_managed_chain, is_missing_chain_error,
    };

    #[test]
    fn stale_cleanup_only_accepts_cpr_chain_names() {
        assert!(is_managed_chain("CPR-0123456789ab"));
        assert!(!is_managed_chain("CPR-0123456789a"));
        assert!(!is_managed_chain("CPR-0123456789abC"));
        assert!(!is_managed_chain("DOCKER-USER"));
        assert!(!is_managed_chain("CPR-0123456789az"));
    }

    #[test]
    fn only_missing_chain_errors_fall_back_to_forward() {
        assert!(is_missing_chain_error(
            b"iptables: chain DOCKER-USER does not exist"
        ));
        assert!(is_missing_chain_error(
            b"iptables: No chain/target/match by that name."
        ));
        assert!(!is_missing_chain_error(
            b"iptables: Permission denied (you must be root)"
        ));
        assert!(!is_missing_chain_error(
            b"iptables: Resource temporarily unavailable"
        ));
    }

    #[test]
    fn only_chain_creation_allows_existing_chain_error() {
        let create = vec![
            "-t".to_owned(),
            "nat".to_owned(),
            "-N".to_owned(),
            "CPR-a".to_owned(),
        ];
        let append = vec![
            "-t".to_owned(),
            "nat".to_owned(),
            "-A".to_owned(),
            "CPR-a".to_owned(),
        ];
        assert!(is_chain_create(&create));
        assert!(is_existing_chain_error(
            &create,
            b"Chain 'CPR-a' already exists."
        ));
        assert!(!is_existing_chain_error(
            &append,
            b"Chain 'CPR-a' already exists."
        ));
        assert!(!is_existing_chain_error(
            &create,
            b"iptables: Permission denied"
        ));
    }
}
