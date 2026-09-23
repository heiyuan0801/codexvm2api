# OpenAI 账号独立槽位：出网模块现状与下一步

> 状态：**进行中（P0 完成，P1 大部分完成）**。出网模块已经接到 Docker 引擎：网络身份会被固定并校验，
> 规则在容器创建/启动前应用，删除时先拆规则再拆网络；首轮对账会清理遗留用户链并记录结构化日志。
> 本地 Docker Desktop 已完成基础部署、sidecar 隔离和容器生命周期验证；剩余工作是指标后端接入和 Linux + Docker
> 宿主网络命名空间真机验收。
>
> 基线：`codex/continue-account-slots` @ `de07e8b3`（本地 Docker 验证与 Host 测试镜像修复已提交）
> 编制日期：2026-09-23

---

## 1. 需求（要做什么）

### 1.1 业务目标

为 `codex-proxy-rs` 的 OpenAI 账号提供**可选的 1:1 Docker 槽位**：一个账号固定对应一个常驻容器、
一个持久状态卷和一条独立网络出口。账号的上游 HTTP/SSE 请求必须经该容器、经该账号绑定的代理发出。

该能力默认关闭，现有账号与部署行为不变。首版只支持 OpenAI Provider，不改动 xAI 路径。

### 1.2 硬性约束

| 编号 | 约束 | 理由 |
| --- | --- | --- |
| C1 | 槽位不可用时账号退出调度，**绝不回退到网关直连** | 回退会让请求以宿主真实出口发出，破坏账号隔离 |
| C2 | 代理是槽位启用的必要条件，缺少代理时槽位不进入 `Ready` | C1 的直接推论 |
| C3 | 槽位容器不得直接访问互联网，**没有出口代理就没有出网** | 槽位存在的意义是稳定且唯一的出口 |
| C4 | 账号凭证、代理 URL、bearer token 不进入环境变量、容器 labels、日志 | 容器元数据对同宿主任何进程可读 |
| C5 | 不模拟 SMBIOS/TPM/磁盘序列；稳定的是软件身份（hostname、machine-id、安装 ID、HOME、时区） | 不是 KVM/VMware/特权容器 |
| C6 | PostgreSQL 仍是账号凭证与期望配置的权威源；运行健康是可恢复观测 | 网关重启后必须能重建 |
| C7 | 工作区禁用 `unsafe`、`warnings` 视为错误 | `unsafe_code = "forbid"`、`warnings = "deny"` |

### 1.3 分层要求

设计文档 `docs/plans/2026-09-22-openai-account-slots-design.md` 已划定完整边界，
实施计划 `docs/plans/2026-09-22-openai-account-slots.md` 把工作切成 8 个任务。
本文只覆盖**任务 4 的一个子集：槽位出网（egress）**，其余任务按原计划推进。

---

## 2. 技术分析

### 2.1 为什么需要在宿主上做透明代理

槽位容器必须"只能用代理出网"，这一点无法靠容器内配置解决：容器内的进程可以忽略 `HTTP_PROXY`
环境变量，也可以直接 `connect()` 到任意地址。唯一可靠的位置是**宿主的网络层**——在槽位容器
自己的网桥上拦截所有出站流量。

因此每个槽位被放进一张**独立网桥**，网桥上的出站流量在 `nat/PREROUTING` 被 REDIRECT 到本进程：

```text
槽位容器
  │  (默认网关 = 网桥网关地址，由本进程绑定)
  ▼
nat/PREROUTING -i cpr<hash>  ──►  CPR-<12hex> 链
                                   │ -d <subnet>          → RETURN（回网关自身）
                                   │ -p udp --dport 53    → REDIRECT :<dns>
                                   │ -p tcp --dport 443   → REDIRECT :<tls>
                                   │ -p tcp --dport 80    → REDIRECT :<http>
                                   └ 其余                  → DROP

filter/DOCKER-USER -i cpr<hash> ──►  同名链
                                   │ -d <subnet>          → RETURN
                                   │ -i br -o br          → DROP（槽位之间不得互通）
                                   └ 其余                 → DROP
```

`filter` 侧同样 DROP 是关键：只做 `nat` 重定向的话，槽位仍能通过网桥转发到宿主的其他网段。

### 2.2 核心难题：REDIRECT 丢掉了目的地址

`REDIRECT` 把目的**地址**改写成本机地址，但**端口保持原值**。于是：

- 443 入口收到的连接，目的端口一定是 443；
- 80 入口收到的连接，目的端口一定是 80；
- 目的主机名则完全丢失。

恢复主机名只能在应用层做，即"从流量本身读出来"：

| 入口 | 恢复方式 | 实现位置 |
| --- | --- | --- |
| TCP 443 | TLS `ClientHello` 的 SNI 扩展 | `egress/sni.rs` |
| TCP 80 | 明文 HTTP 请求头 `Host`，或 `CONNECT` 的 authority | `egress/sni.rs` |
| UDP 53 | 不需要：被接管的名字返回合成地址，其余原样转发 | `egress/dns.rs` |

`SO_ORIGINAL_DST` 本可以拿回原始目的地址，但它需要 `unsafe`，工作区禁用，因此这条路不可用。
这不是妥协——它反而强制了更安全的语义：**端口只能来自监听器身份**。

### 2.3 只接管 53/443/80

这是刻意的取舍。REDIRECT 之后没有任何可信的端口号，只能按"哪个监听器收到的"推断端口。
其余端口（22、25、3306、任意高端口）若也 REDIRECT，就只能按错误端口连出——那比拒绝更糟。
因此计划里明确写 `-j DROP` 兜底：

- 好处：不存在"按错误端口连出"的失败模式；
- 代价：槽位无法访问非标端口（如自建 API 的 `8443`）。这是可接受的，因为槽位的用途是访问 OpenAI 上游。

### 2.4 DNS 协商：合成地址 + 白名单后缀

DNS 入口不解析所有名字。只有配置的后缀（`INTERCEPT_SUFFIXES`）返回**合成地址**
（`203.0.113.0/24`，RFC 5737 文档用段），其余查询原样转发给 `1.1.1.1:53`。

设计动机是让 TLS 入口一定能看到 SNI：如果槽位直连到真实 IP，`ClientHello` 依然有 SNI，
但如果槽位在解析阶段就被本地劫持（例如容器内 `resolv.conf` 指向别的服务器），
拦截就失效了。

### 2.5 出口代理：三种方案，全部 fail-closed

`egress/dial.rs` 实现 `connect_via`，支持：

- **HTTP `CONNECT`**（含 `Proxy-Authorization: Basic`，手写 base64 以避免引入依赖）
- **SOCKS5 域名模式（`socks5h`）**：主机名交给代理解析，避免宿主 DNS 泄露槽位要访问的名字
- **SOCKS5 本地解析（`socks5`）**：本地解析后提交 IPv4 字面量；解析失败即失败，不兜底

`https` 代理**在使用前被拒绝**（`DialError::TlsProxyUnsupported`）：模块内没有 TLS 客户端，
按明文连接会把 `CONNECT` 和 `Basic` 凭据明文发给一个期待 TLS 的端口——既连不通，又泄露凭据。

### 2.6 身份推导必须是纯函数

槽位容器的重启策略是 `UNLESS_STOPPED`：容器崩溃后 Docker 会自动拉起它，**不会**再走一次 `converge`。
这意味着网桥名、子网、链名必须在容器重建后保持不变，否则重启后的容器会落在没有规则的网络上。

因此三个身份全部由实例 ID 纯函数推导：

| 身份 | 函数 | 形态 |
| --- | --- | --- |
| 网桥接口名 | `net::bridge_name` | `cpr` + FNV-1a 的 8 位十六进制，共 11 字节（Linux 上限 15） |
| 子网 | `net::subnet_for` | `172.24.<hash % 256>.0/24` |
| 用户链名 | `EgressTarget::chain` | `CPR-` + 12 位十六进制 |

**取散列而不是取前缀**是一个已修复的缺陷：UUIDv7 的高位就是毫秒时间戳，截断前缀会让同一毫秒内
创建的所有槽位映射到同一个网桥/子网/链——两个槽位共用一张网桥会互相覆盖 `iptables` 规则，
共用一个子网会让第二个网络的创建因地址池冲突而失败。
`net.rs` 与 `plan.rs` 里各有一个回归测试专门锁住这一点。
散列还必须覆盖**完整**的 128 位，只在同一毫秒内不同的两个 ID（只差时间戳低位与计数器）才会被分开。

子网池选 `172.24.0.0/16`：Docker 默认地址池是 `172.17.0.0/16` 与 `192.168.0.0/16`，
避开它们可以让槽位网络在默认配置下不与既有网络冲突。
已知残留风险：宿主若已占用 `172.24.x.x`，会与槽位网络冲突——目前该段是硬编码的，无配置项。

### 2.7 端口分配交给内核

入口的三个端口用 `0` 绑定，由内核分配，再从 `local_addr()` 读回（`RedirectPorts::EPHEMERAL` +
`SlotEgress::ports()`）。这比自建分配器可靠——内核只会给出真正空闲的端口；
也让 `OpenAiSlotsConfig` 不需要新增任何端口字段。

推论：**规则只能在绑定之后生成**，所以 `ensure`（建入口）与 `apply_rules`（下发规则）是两个调用。

### 2.8 监听器绑定网关地址而非 `0.0.0.0`

REDIRECT 改写后的连接一定落在**网桥网关地址**上（`net::gateway_of` 推导的子网第一个可用地址）。
绑定该具体地址可以关掉一个残留暴露面：宿主上其他进程无法直接连到入口端口、借槽位的出口代理上网。

### 2.9 fail-closed 不变量

`transport.rs::handle_connection` 是唯一的分流点，以下情况一律**断开**，绝不直连：

- 恢复不出主机名（无 SNI / 无 `Host` / 报文不可识别 / 超时）
- 槽位没有出口代理（`config.proxy == None`）
- 出口代理拒绝连接、方案不受支持、超时

集成测试 `crates/gateway-host/tests/slots/egress.rs` 用真实 TCP 对 8 个场景做了端到端断言，
其中 4 个是"必须被关闭且代理侧看不到任何连接"的负向场景。

---

## 3. 项目分析

### 3.1 代码结构

```text
backend/crates/gateway-host/src/slots/
├── mod.rs            20    模块导出
├── model.rs          58    期望状态 / 健康快照（手写 redacting Debug）
├── ports.rs          53    AccountSlotEngine 等 trait
├── reconciler.rs    149    一次对账的收敛策略
├── registry.rs      190    路由注册表 + 健康观测
├── docker.rs        779    Bollard 适配器（容器/网络/卷）
├── source.rs         89    期望状态来源
├── worker.rs         72    周期对账任务
├── bundle.rs         52    依赖装配
└── egress/                 ★ 本模块
    ├── mod.rs        45    EgressError 与导出
    ├── net.rs       182    网桥名 / 子网 / 网关推导
    ├── plan.rs      280    iptables 计划的纯函数推导
    ├── iptables.rs   84    规则执行抽象（trait + 命令实现）
    ├── sni.rs       328    TLS SNI / HTTP Host 解析
    ├── dns.rs       265    DNS 问询解析与应答构造
    ├── dial.rs      322    HTTP CONNECT / SOCKS5 / base64
    ├── transport.rs 321    三个转发入口与分流
    └── manager.rs   174    ★ 每槽入口与规则的生命周期
```

测试：`tests/slots/` 下 5 个文件；`src/slots/` 内有 26 个单元测试；
`tests/slots/egress.rs` 有 8 个端到端测试。

### 3.2 各层完成度

| 层 | 内容 | 状态 |
| --- | --- | --- |
| 数据库 | `0017`–`0020` 四张迁移：槽位意图、代理代次、独立槽位、删除意图 | ✅ 完成 |
| gateway-core | `AccountSlotInstanceId` / `OutboundProxy` / `AccountSlotRoute` 等 | ✅ 完成 |
| gateway-store | 槽位读写与 CAS 更新、级联删除 | ✅ 完成 |
| gateway-host 对账 | 引擎 trait、对账器、注册表、周期任务、Docker 适配器 | ✅ 完成 |
| **gateway-host 出网** | **egress 全部子模块 + manager + Docker 生命周期接线** | ✅ P0 完成 |
| gateway-admin / api | 槽位管理用例与 Admin 路由 | ✅ 已实现 |
| gateway-providers/openai | Slot Transport（任务 5） | ✅ 已实现 |
| frontend | 槽位开关与状态标签（任务 7） | ✅ 已实现 |
| deploy | sidecar 镜像、compose、验证脚本（任务 8） | ✅ 已实现，待 Linux 验收 |

### 3.3 当前构建与测试状态

本次接线后的验证以 `+1.97.0` 工具链运行：`cargo check -p gateway-host --all-targets --locked` 通过，
Docker 生命周期测试 `cargo test -p gateway-host --test main slots::docker --locked` 为 4 passed。
完整 `gateway-host` 测试以 `--test-threads=1` 运行：126 passed、1 ignored。

本地 Docker Desktop 验证结果：默认 Compose 的 PostgreSQL、Redis 均为 healthy，当前源码网关镜像的 `/healthz`
返回 204、首页返回 200；`verify-account-slots.py` 的双槽位隔离检查通过，覆盖资源与身份隔离、独立代理出口、
bearer 鉴权、凭据不转发、密钥文件权限、inspect/日志脱敏、容器重建和代理故障隔离；
`verify-account-slot-host.py` 的真实 Docker 生命周期测试 1 passed。

将 `compose.slots.yaml` 直接叠加到 macOS Docker Desktop 时，网关槽位对账会因无法访问宿主网络命名空间的
`iptables` 而保持不健康。这是 Docker Desktop 网络能力限制；完整出网规则和宿主网桥仍需在 Linux + Docker
环境中验收。基础无槽位 Compose 不受影响。

### 3.4 未完成的部分（本模块）

P0 接线前的缺口已完成。当前实现要点如下：

1. `docker.rs::ensure_network` 创建网络时显式设置 `com.docker.network.bridge.name`、派生 IPAM `/24` 和
   `internal: true`，复用前校验 owner、driver、bridge、IPAM 和 internal，不匹配直接 fail closed。

2. `BollardAccountSlotEngine` 持有可注入的 `SlotEgressLifecycle`；生产连接使用
   `ManagedSlotEgress(Arc<CommandIptables>)`，测试可以替换端口和规则执行器。

3. `converge` 顺序固定为 volume → network → egress ensure → egress apply → create/start container；
   `delete` 在移除容器和网络前拆除 egress，`stop` 不会拆除入口。

4. Docker 假 API 返回完整网络身份，测试替身记录并断言规则阶段先于容器创建；已删除无调用方的
   `net::LISTEN_ADDR`。

---

## 4. 技术决策记录（含被否决的方案）

| 决策 | 选择 | 否决的替代方案与原因 |
| --- | --- | --- |
| 拦截点 | 宿主 `iptables` NAT | 容器内代理环境变量：进程可忽略，不可强制 |
| 原始目的地址 | 从流量恢复（SNI/Host） | `SO_ORIGINAL_DST`：需要 `unsafe`，工作区禁用 |
| 未接管端口 | `DROP` | 全部 REDIRECT 到单一端口：拿不到可信端口号，会按错误端口连出 |
| 网络身份 | 实例 ID 的 FNV 散列 | 取 UUIDv7 前缀：同一毫秒内的槽位会撞名/撞子网 |
| 子网分配 | 纯函数推导 | 交给 Docker 分配再读回：需要额外状态，且规则生成要等到读回之后 |
| 端口分配 | 内核分配 + 读回 | 自建分配器：需要持久状态，且无法感知宿主其他进程占用 |
| 监听地址 | 网桥网关地址 | `0.0.0.0`：宿主其他进程可直接连入口，借槽位代理出网 |
| 槽位容器重启 | 入口跨对账周期存活 | 每次对账重建：容器被 Docker 自动拉起后没有出网路径 |
| `https` 代理 | 使用前拒绝 | 明文连 TLS 端口：连不通且泄露 `Basic` 凭据 |

---

## 5. 下一步规划

按依赖顺序排列，每步都可独立验证。

### P0 — 接通出网（已完成）

1. **`ensure_network` 显式钉住网络身份** ✅
   - `options: { "com.docker.network.bridge.name": egress::bridge_name(instance) }`
   - `ipam: { config: [{ subnet: egress::subnet_for(instance) }] }`
   - `internal: Some(true)`——即便规则尚未下发，槽位也没有直连路径
   - 从 `NetworkInspect` 读回 `options` / `ipam` 并**校验**，不匹配则 fail closed
     （`NetworkInspect` 已确认同时暴露 `ipam` 与 `options` 字段，可以真校验而非仅凭推测）

2. **`BollardAccountSlotEngine` 持有 `SlotEgressManager`** ✅
   - `connect` 里用 `Arc::new(CommandIptables)` 构造
   - 通过 `SlotEgressLifecycle` 保持并发安全，`Debug` 不输出代理或凭证

3. **`converge` 的顺序**：`ensure_volume` → `ensure_network` → `egress.ensure` → `egress.apply_rules`
   → 之后才创建/启动容器。**容器不能先于规则存在**，否则启动瞬间是裸奔的。

4. **`delete` 的顺序**：`egress.remove` **先于** `remove_network`。 ✅
   网桥不存在时 `-i <bridge>` 匹配的规则会因缺少接口而删除失败，留下悬空跳转。

5. **`stop` 保留入口**：容器 `UNLESS_STOPPED` 会自动重启，入口必须活着。 ✅

6. **测试**： ✅
   - 假 Docker API 补齐 `Options` / `IPAM` 返回，并更新请求计数断言；
   - 注入假的 egress lifecycle，断言**规则阶段发生在 `create_container` 之前**；生产实现仍使用
     `CommandIptables`，Linux 真机验收再覆盖 `delete` 到 `remove_network` 的顺序。

7. `net::LISTEN_ADDR` 已删除。 ✅

### P1 — 可观测性与运维安全（大部分完成）

- 已补齐 `slot_egress` 结构化日志：入口建立、规则应用/拆除失败、代理连接失败、DNS 转发失败和遗留链清理；
- 首轮 `list_owned` 对账（或首次槽位 ensure/remove）会严格清理 `CPR-` + 12 位十六进制用户链，
  先删跳转再 flush/delete，失败则阻止槽位继续收敛；
- 当前仓库没有 metrics 后端，代理拒绝率和 DNS 失败率暂以结构化日志字段记录，后续接入指标系统。

### P2 — 其余任务（代码已完成，待环境验收）

- 任务 5：OpenAI Provider 的 Slot Transport 已接入；
- 任务 6/7：Admin、API 与前端槽位状态已接入；
- 任务 8：sidecar 镜像、compose 和验证脚本已加入；本地 Docker Desktop 已通过 sidecar 隔离与 Host
  生命周期测试，Host 验收脚本已补齐 `iptables`、宿主网络命名空间和 `NET_ADMIN`/`NET_RAW` 能力，待 Linux +
  Docker 双槽位出网验收。

### P3 — 已知的技术债

- `172.24.0.0/16` 硬编码，无配置项；宿主若已占用会冲突；
- 非标端口（如 `8443`）无法访问；
- `Iptables::run` 刻意忽略退出码，重复 `-I` 可能累积跳转。当前靠"先 `-F` 再 `-I`"缓解，
  但没有断言**跳转本身**只有一条；
- `https` 代理不支持（需要引入 TLS 客户端）。

---

## 6. 验证方式

```bash
cargo check -p gateway-host --all-targets
cargo test  -p gateway-host
cargo clippy -p gateway-host --all-targets -- -D warnings
cargo fmt --check
```

出网模块单独跑：

```bash
cargo test -p gateway-host --test main slots::egress
```

当前 Docker 测试已覆盖：

- `create_container` 之前已下发全部 `CPR-*` 规则；
- 网络读回会校验网桥名、推导子网和 `internal == true`；
- 外部资源在任何写操作前返回 `Unauthorized`。

本地 Docker 验证脚本：

```bash
CPR_SLOT_IMAGE=codex-slot-sidecar:local python3 deploy/tests/verify-account-slots.py
CPR_SLOT_IMAGE=codex-slot-sidecar:local \
CPR_SLOT_HOST_TEST_IMAGE=codex-slot-host-tests:local \
python3 deploy/tests/verify-account-slot-host.py
```

Linux + Docker 验收还需补充网络创建请求字段和 `delete` 时规则拆除早于 `remove_network` 的端到端断言。

---

## 7. 风险登记

| 风险 | 影响 | 缓解 |
| --- | --- | --- |
| 网桥名未钉住，规则永不命中 | 槽位直连出网，隔离失效 | P0-1；`internal: true` 作为第二道防线 |
| `internal: true` 与网关容器共存 | 网关自己也在该网络上，需要能访问 sidecar | 已验证网桥内部互通不受 `internal` 影响；需在真机验证 |
| 宿主已占用 `172.24.x.x` | 网络创建失败，槽位无法启动 | 记录为已知限制；后续做成配置项 |
| 进程崩溃遗留 `CPR-*` 链 | 重启后规则指向已消失的监听端口 | P1 的启动清理；Linux 真机仍需验收清理顺序 |
| 非容器环境无法验证 | Windows 上只能验证纯逻辑与假引擎 | 集成验证必须放到 Linux + Docker 环境 |
| Docker Desktop 无宿主网络命名空间 | macOS 叠加槽位 Compose 时 iptables 对账失败 | 使用 Linux Docker 主机完成宿主网桥和出网规则验收；基础 Compose 可正常运行 |
