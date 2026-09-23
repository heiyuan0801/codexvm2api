//! `iptables` 规则的纯函数推导。
//!
//! 与 vm2api 的 `kin-egress` 同形：每个槽位一条私有网桥，网桥关闭 MASQUERADE，
//! 槽位容器出网方向的 TCP 全部 REDIRECT 到本进程的透明转发入口，
//! 转发入口再按 SNI / Host 经槽位代理连出；网桥不做转发，未匹配流量一律丢弃。

/// 本进程为某槽位建立的转发入口端口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RedirectPorts {
    pub dns: u16,
    pub tls: u16,
    pub http: u16,
}

impl RedirectPorts {
    /// 交给内核分配端口；绑定的实际端口从监听器读回。
    pub const EPHEMERAL: Self = Self {
        dns: 0,
        tls: 0,
        http: 0,
    };
}

/// 单个槽位的网络身份。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EgressTarget {
    /// 桥接接口名。
    pub bridge: String,
    /// 槽位网络的 CIDR，例如 `172.29.0.0/16`。
    pub subnet: String,
    pub ports: RedirectPorts,
}

/// `iptables` 用户链名的十六进制位数；`CPR-` + 12 位共 16 字节，远低于 28 字节上限。
const CHAIN_DIGITS: usize = 12;

/// 转发入口承接的 TLS 目的端口，也是重定向后连接的目标端口。
pub const TLS_PORT: u16 = 443;
/// 转发入口承接的明文 HTTP 目的端口。
pub const HTTP_PORT: u16 = 80;

impl EgressTarget {
    /// `iptables` 用户链名。
    ///
    /// 与 [`super::net::bridge_name`] 同样取完整实例 ID 的散列：UUIDv7 前缀是时间戳，
    /// 按前缀取 12 位只有毫秒级分辨率，同一毫秒创建的两个槽位会共用一条链。
    #[must_use]
    pub fn chain(&self, instance: impl std::fmt::Display) -> String {
        let raw = instance.to_string();
        let hash = super::net::instance_hash(&raw);
        format!("CPR-{}", super::net::hex_prefix(hash, CHAIN_DIGITS))
    }
}

/// `filter` 表里承载槽位转发的父链。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardParent {
    /// 存在 `DOCKER-USER` 时优先使用，避免与 `Docker` 自身规则顺序耦合。
    DockerUser,
    /// `DOCKER-USER` 不存在时回落到 `FORWARD` 首条。
    Forward,
}

impl ForwardParent {
    #[must_use]
    pub const fn chain(self) -> &'static str {
        match self {
            Self::DockerUser => "DOCKER-USER",
            Self::Forward => "FORWARD",
        }
    }
}

/// 一条待执行的 `iptables` 命令（不含 `iptables` 本身与 `-w`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule(pub Vec<String>);

/// 幂等下发槽位出网规则所需的有序命令序列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulePlan {
    pub apply: Vec<Rule>,
    /// 仅用于删除跳转，链本身由 `teardown` 负责。
    pub teardown: Vec<Rule>,
}

fn nat(args: &[&str]) -> Rule {
    let mut owned = vec!["-t".to_owned(), "nat".to_owned()];
    owned.extend(args.iter().map(|arg| (*arg).to_owned()));
    Rule(owned)
}

fn filter(args: &[&str]) -> Rule {
    let mut owned = vec!["-t".to_owned(), "filter".to_owned()];
    owned.extend(args.iter().map(|arg| (*arg).to_owned()));
    Rule(owned)
}

/// 生成槽位出网的 `iptables` 计划。
///
/// `nat/PREROUTING` 在 `-i <bridge>` 上跳到槽位链：到本网段的流量 RETURN，
/// UDP 53 重定向到内建解析器，TCP 443 重定向到 TLS 分流器，
/// TCP 80 重定向到 HTTP 分流器，最后 `DROP` 兜底。
/// `filter` 在 `DOCKER-USER`（或 `FORWARD`）上跳到槽位链：只允许网关自身取回流量，
/// 槽位容器之间的转发一律丢弃。
///
/// 只接管 53/443/80 是刻意的：REDIRECT 会改写目的地址，被转发的连接拿不回原始
/// 目的端口（`SO_ORIGINAL_DST` 需要 `unsafe`，本工作区禁用），因此入口只能按
/// 「哪个监听器收到的」来推断端口。其余端口没有可信的端口号可用，与其按错误端口
/// 连出，不如按 [`DROP`] 拒绝。
#[must_use]
pub fn plan(target: &EgressTarget, chain: &str, parent: ForwardParent) -> RulePlan {
    let bridge = target.bridge.as_str();
    let subnet = target.subnet.as_str();
    let dns = target.ports.dns.to_string();
    let tls = target.ports.tls.to_string();
    let http = target.ports.http.to_string();
    let mut apply = vec![
        // 用户链幂等重建：确保存在后清空，再重放规则。
        nat(&["-N", chain]),
        nat(&["-F", chain]),
        nat(&["-A", chain, "-d", subnet, "-j", "RETURN"]),
        nat(&[
            "-A",
            chain,
            "-p",
            "udp",
            "--dport",
            "53",
            "-j",
            "REDIRECT",
            "--to-ports",
            &dns,
        ]),
        nat(&[
            "-A",
            chain,
            "-p",
            "tcp",
            "--dport",
            "443",
            "-j",
            "REDIRECT",
            "--to-ports",
            &tls,
        ]),
        nat(&[
            "-A",
            chain,
            "-p",
            "tcp",
            "--dport",
            "80",
            "-j",
            "REDIRECT",
            "--to-ports",
            &http,
        ]),
        // ICMP 等无法恢复原始目的地址的流量没有安全出口，直接丢弃。
        nat(&["-A", chain, "-j", "DROP"]),
        // 槽位桥上的入向流量必须在 PREROUTING 最前面被接管，否则会先被 Docker 的
        // DNAT 规则改写，槽位就失去了唯一的出网路径。
        nat(&["-I", "PREROUTING", "1", "-i", bridge, "-j", chain]),
        filter(&["-N", chain]),
        filter(&["-F", chain]),
        // 到本网段的流量（含网关自身）不属于出网，交给后续 DOCKER 规则。
        filter(&["-A", chain, "-d", subnet, "-j", "RETURN"]),
        // 槽位容器之间不得互通。
        filter(&["-A", chain, "-i", bridge, "-o", bridge, "-j", "DROP"]),
        // 仅放行回到槽位容器的入向连接，其余出口一律交给转发入口。
        filter(&["-A", chain, "-i", bridge, "-j", "DROP"]),
    ];
    let parent_chain = parent.chain();
    apply.push(filter(&[
        "-I",
        parent_chain,
        "1",
        "-i",
        bridge,
        "-j",
        chain,
    ]));
    let teardown = vec![
        nat(&["-D", "PREROUTING", "-i", bridge, "-j", chain]),
        filter(&["-D", parent_chain, "-i", bridge, "-j", chain]),
        nat(&["-F", chain]),
        nat(&["-X", chain]),
        filter(&["-F", chain]),
        filter(&["-X", chain]),
    ];
    RulePlan { apply, teardown }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 测试用实例 ID。接口名与链名都由它推导，两边不会各写一份字面量。
    const INSTANCE: &str = "slot_0199f4c8-52a8-7000-8000-000000000001";

    fn target() -> EgressTarget {
        EgressTarget {
            bridge: super::super::net::bridge_name(INSTANCE),
            subnet: "172.29.0.0/16".to_owned(),
            ports: RedirectPorts {
                dns: 41000,
                tls: 41001,
                http: 41002,
            },
        }
    }

    #[test]
    fn chain_name_is_stable_and_bounded() {
        let target = target();
        let chain = target.chain(INSTANCE);
        assert!(chain.len() <= 28, "链名上限 28 字节: {chain}");
        assert_eq!(chain.len(), "CPR-".len() + CHAIN_DIGITS);
        assert!(chain[4..].chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(chain, target.chain(INSTANCE));
    }

    #[test]
    fn chain_names_differ_within_the_same_millisecond() {
        // 回归：按 UUIDv7 前缀取 12 位只有毫秒分辨率，同一毫秒的槽位会共用一条链，
        // 后建的槽位 flush 掉先建的规则后两者就都失去出网约束。
        let target = target();
        let first = target.chain("slot_0199f4c8-52a8-7000-8000-000000000001");
        let second = target.chain("slot_0199f4c8-52a8-7000-8000-000000000002");
        assert_ne!(first, second);
    }

    #[test]
    fn plan_redirects_every_tcp_port_and_keeps_dns() {
        let target = target();
        let plan = plan(&target, &target.chain(INSTANCE), ForwardParent::DockerUser);
        let rendered = plan
            .apply
            .iter()
            .map(|rule| rule.0.join(" "))
            .collect::<Vec<_>>();
        assert!(
            rendered
                .iter()
                .any(|rule| rule.contains("--dport 53")
                    && rule.contains("REDIRECT --to-ports 41000"))
        );
        assert!(
            rendered
                .iter()
                .any(|rule| rule.contains("--dport 443")
                    && rule.contains("REDIRECT --to-ports 41001"))
        );
        assert!(
            rendered
                .iter()
                .any(|rule| rule.contains("--dport 80")
                    && rule.contains("REDIRECT --to-ports 41002"))
        );
        // 除 53/443/80 外的 TCP 必须落到 DROP：入口拿不回原始目的端口，
        // 放行只会按错误的端口连出。
        assert!(
            !rendered
                .iter()
                .any(|rule| rule.contains("-p tcp -j REDIRECT"))
        );
        assert!(rendered.iter().any(|rule| rule.ends_with("-j DROP")));
        // 期望值由 `target`/`chain` 推导，命名方案变化时这里不会变成过期字面量。
        assert_eq!(
            rendered.last().unwrap(),
            &format!(
                "-t filter -I DOCKER-USER 1 -i {} -j {}",
                target.bridge,
                target.chain(INSTANCE)
            )
        );
    }

    #[test]
    fn teardown_removes_jumps_before_chains() {
        let target = target();
        let chain = target.chain(INSTANCE);
        let plan = plan(&target, &chain, ForwardParent::Forward);
        let rendered = plan
            .teardown
            .iter()
            .map(|rule| rule.0.join(" "))
            .collect::<Vec<_>>();
        // 先摘跳转、再清空并删除两条用户链；顺序反了会留下悬空跳转。
        assert_eq!(
            rendered[0],
            format!("-t nat -D PREROUTING -i {} -j {chain}", target.bridge)
        );
        assert_eq!(
            rendered[1],
            format!("-t filter -D FORWARD -i {} -j {chain}", target.bridge)
        );
        assert_eq!(rendered[2], format!("-t nat -F {chain}"));
        assert_eq!(rendered[3], format!("-t nat -X {chain}"));
    }
}
