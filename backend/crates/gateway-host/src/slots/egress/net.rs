//! 槽位桥接网络的地址推导；不依赖 Docker 类型，便于单独测试。

use std::net::{IpAddr, Ipv4Addr};

/// Linux 接口名上限 15 字节，且首字符必须是字母。
pub const BRIDGE_PREFIX: &str = "cpr";
/// 桥接口名中的十六进制位数。
const BRIDGE_DIGITS: usize = 8;

/// FNV-1a 64 位偏移基与质数；用于把完整实例 ID 压成定长名字。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// 对完整实例 ID 做 FNV-1a 散列。
///
/// 取散列而不是取前缀：UUIDv7 的高位就是毫秒时间戳，截断前缀等同于把
/// 同一时间窗内创建的所有槽位映射到同一个名字（前 8 位十六进制只有约 49 天分辨率），
/// 那样多个槽位会共用一张网桥并互相覆盖 `iptables` 规则。
/// 散列覆盖全部 128 位，名字才是真正的实例身份。
#[must_use]
pub(crate) fn instance_hash(instance: &str) -> u64 {
    let mut hash = FNV_OFFSET;
    for byte in instance.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// 从散列高位生成定长小写十六进制串。
///
/// 取高位而非低位：低位在同一毫秒内可能被 v7 的计数器影响，高位混合更充分。
#[must_use]
pub(crate) fn hex_prefix(hash: u64, digits: usize) -> String {
    let shift = 64 - digits * 4;
    format!("{:0width$x}", hash >> shift, width = digits)
}

/// 由实例 ID 推导稳定的桥接接口名：`cpr` + 8 位十六进制，共 11 字节。
///
/// 槽位网络按实例隔离，同一实例重建后接口名不变，规则可以幂等覆盖。
#[must_use]
pub fn bridge_name(instance: impl std::fmt::Display) -> String {
    let raw = instance.to_string();
    format!("{BRIDGE_PREFIX}{}", hex_prefix(instance_hash(&raw), BRIDGE_DIGITS))
}

/// 槽位网段池；每个槽位从中切出一个 `/24`。
///
/// 选择 `172.24.0.0/16`：`Docker` 默认地址池是 `172.17.0.0/16` 与
/// `192.168.0.0/16`，避开这两段可以让槽位网络在默认配置下不与既有网络冲突，
/// 也避免与常见内网段（`10.0.0.0/8`、`192.168.x.x`）撞车。
pub const SUBNET_POOL: Ipv4Addr = Ipv4Addr::new(172, 24, 0, 0);
/// 池内可分配的 `/24` 个数。
pub(crate) const SUBNET_POOL_SIZE: u32 = 256;

/// 由实例 ID 推导该槽位的 `/24` 子网。
///
/// 必须是纯函数：`iptables` 计划里的 `-d <subnet>` 与网关绑定都要用同一个值，
/// 而且容器重建后子网不能变，否则旧规则会指向不存在的网段。
/// 用散列而不是递增分配，是为了让子网不依赖分配顺序，
/// 从而可以离线推导、可在测试里断言。
#[must_use]
pub fn subnet_for(instance: impl std::fmt::Display) -> String {
    let raw = instance.to_string();
    let index = u32::try_from(instance_hash(&raw) % u64::from(SUBNET_POOL_SIZE)).unwrap_or(0);
    let octets = SUBNET_POOL.octets();
    let third = u8::try_from(index).unwrap_or(0);
    format!("{}.{}.{}.0/24", octets[0], octets[1], third)
}

/// 解析 `Docker` IPAM 返回的点分 IPv4 地址。
#[must_use]
pub fn parse_ipv4(value: &str) -> Option<Ipv4Addr> {
    value.trim().parse::<Ipv4Addr>().ok()
}

/// 解析 `a.b.c.d/len`，只接受 IPv4 且前缀长度合法。
#[must_use]
pub fn parse_ipv4_subnet(value: &str) -> Option<(Ipv4Addr, u8)> {
    let (address, prefix) = value.split_once('/')?;
    let address = parse_ipv4(address)?;
    let prefix = prefix.trim().parse::<u8>().ok()?;
    (prefix <= 32).then_some((address, prefix))
}

/// 取子网掩码。
#[must_use]
pub const fn subnet_mask(prefix: u8) -> Ipv4Addr {
    let bits = if prefix >= 32 {
        u32::MAX
    } else if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    let octets = bits.to_be_bytes();
    Ipv4Addr::new(octets[0], octets[1], octets[2], octets[3])
}

/// 从 IPAM 子网推导网关地址（子网第一个可用地址）。
///
/// Docker `bridge` 驱动默认把网关放在子网的第一个地址，槽位容器也只使用这一个地址，
/// 因此反向推导不会与 Docker 分配冲突。
#[must_use]
pub fn gateway_of(subnet: &str) -> Option<Ipv4Addr> {
    let (address, prefix) = parse_ipv4_subnet(subnet)?;
    let mask = u32::from(subnet_mask(prefix));
    let network = u32::from(address) & mask;
    let gateway = network.checked_add(1)?;
    (u32::from(address) & mask == network && prefix <= 30).then_some(Ipv4Addr::from(gateway))
}

/// 转发监听地址：覆盖全部 IPv4 接口。
///
/// 重定向由 `iptables` REDIRECT 完成，只有桥接口上的流量会被送达这里，
/// 因此无需绑定具体网卡地址。
pub const LISTEN_ADDR: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bridge_name_is_stable_and_bounded() {
        let name = bridge_name("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
        assert!(name.len() <= 15);
        assert_eq!(name.len(), BRIDGE_PREFIX.len() + BRIDGE_DIGITS);
        assert!(name.starts_with(|character: char| character.is_ascii_alphabetic()));
        assert!(name[BRIDGE_PREFIX.len()..].chars().all(|c| c.is_ascii_hexdigit()));
        // 同一实例重复推导必须得到同一接口名，规则才能幂等覆盖。
        assert_eq!(name, bridge_name("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d"));
    }

    #[test]
    fn bridge_names_differ_within_the_same_millisecond() {
        // 回归：UUIDv7 的高位是时间戳，取前缀会让同一毫秒内的槽位共用一个接口名。
        // 这两个 ID 只差时间戳低位与计数器，前缀完全相同，散列必须把它们分开。
        let first = "slot_0199f4c8-52a8-7000-8000-000000000001";
        let second = "slot_0199f4c8-52a8-7000-8000-000000000002";
        assert_eq!(&first[5..21], &second[5..21], "两个 ID 的前缀本就相同");
        assert_ne!(bridge_name(first), bridge_name(second));
    }

    #[test]
    fn subnet_is_stable_and_inside_the_pool() {
        let subnet = subnet_for("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d");
        assert_eq!(subnet, subnet_for("slot_0199f4c8-52a8-7aa0-a6d7-f75219e82e3d"));
        let (address, prefix) = parse_ipv4_subnet(&subnet).unwrap();
        assert_eq!(prefix, 24);
        assert_eq!(address.octets()[..2], SUBNET_POOL.octets()[..2]);
        assert_eq!(address.octets()[3], 0);
        // 池必须容得下每个可能的第三段。
        assert!(usize::from(address.octets()[2]) < usize::try_from(SUBNET_POOL_SIZE).unwrap());
        assert!(gateway_of(&subnet).is_some());
    }

    #[test]
    fn subnets_differ_within_the_same_millisecond() {
        // 与接口名同理：按 UUIDv7 前缀分配会让同一毫秒的槽位共用一个子网，
        // 第二个槽位创建网络时会因地址池冲突而失败。
        assert_ne!(
            subnet_for("slot_0199f4c8-52a8-7000-8000-000000000001"),
            subnet_for("slot_0199f4c8-52a8-7000-8000-000000000002")
        );
    }

    #[test]
    fn subnet_derives_expected_gateway() {
        assert_eq!(
            gateway_of("172.29.0.0/16"),
            Some(Ipv4Addr::new(172, 29, 0, 1))
        );
        assert_eq!(
            gateway_of("10.8.4.0/24"),
            Some(Ipv4Addr::new(10, 8, 4, 1))
        );
        assert_eq!(gateway_of("172.29.0.0"), None);
        assert_eq!(gateway_of("172.29.0.0/33"), None);
        assert_eq!(gateway_of("not-a-subnet"), None);
    }
}
