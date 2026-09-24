# OpenAI 独立账号槽位开发交接

## 当前优先需求：先建槽位，再绑定账号

用户已纠正操作流程：参照 `dofastted/vm2api`，先创建独立容器槽位，再设置代理 IP、绑定 OpenAI 账号，最后启动。下文第 7、8 项的账号编辑框开关只是既有实现记录，不能视为该需求已完成，也不能继续作为唯一入口。

已核对参考仓库提交 `65c291bd2a8be8585ea19700ce8db042c9901858` 的前端导入向导、创建/代理绑定/凭据导入/启停 API 和 Docker runtime：导入向导创建空槽使用 `defaultAfter='idle'`，保存独立槽位配置、身份和 HOME；真正的 Docker 容器在启动路径创建。Claude 凭据导入可按需启动 worker，Codex 导入走另一条实现，不应把所有账号导入都解释成同一种启动行为。本次只完成源码核对，未运行参考项目。

本轮实施目标：

- 独立的容器管理入口及“创建 → 配置代理 → 绑定账号 → 启动”流程，允许空槽存在。
- 槽位拥有稳定 ID、身份、HOME 和代理配置；账号作为可选的一对一绑定，运行意图与账号调度开关分开。
- 单独提供创建、代理配置、绑定/解绑、启动、停止和删除操作，明确尚未创建 Docker 容器、已停止、启动中、运行中和失败状态。
- 新增迁移保留现有实例与身份；不能直接修改冻结迁移 0017/0018。现有 `account_id` 主键及级联删除不足以表达独立槽位生命周期。
- 复用现有 sidecar、Docker adapter、Ready registry 和 fail-closed Provider 路由；停槽立即撤销路由，不把已绑定槽位的账号回退为直连。
- 验收必须覆盖无账号创建空槽、配置代理、绑定账号、真实 Docker 启停、故障隔离及重建保留身份。此前账号开关测试通过不能替代这条流程验收。

### 本轮槽位优先改造（本地实现与聚焦验收完成）

- 新增 0019 迁移：槽位改以 instance UUID 为主键，账号允许为空，独立引用代理库；删除账号保留空槽、身份和 HOME。
- 新增独立容器查询/配置接口，支持创建、选择代理、绑定/解绑、启动/停止和删除槽位；generation 防止旧页面覆盖新配置，一对一绑定由数据库唯一约束保证。
- Host 使用槽位代理，Provider 对已绑定但已停止的槽位 fail closed；运行意图与 Docker 停机观测分开展示。
- 新增 `/containers` 页面和侧栏入口；配置弹窗按代理、账号、启动排列。代理添加和账号导入/授权复用现有管理页，返回槽位后完成绑定。
- 旧账号编辑框开关已移除，原更新接口拒绝写入并要求迁移到容器管理；账号列表查询保留。
- 真实 PostgreSQL、Admin/API、Host 聚焦回归 28 项通过；Provider 防直连回归 4 项通过；未变化的架构测试编译产物检查当前源码 12 项通过。真实 Docker 用例在普通测试中明确忽略，已另用 Linux 测试镜像执行并通过 1 项生命周期测试。
- 严格 Clippy、Rustfmt、冻结迁移校验、前端 ESLint/类型检查/构建通过；真实 HTTP 创建/配置/绑定/冲突检查与浏览器创建、代理保存、绑定回显已验收。
- 真实 Docker 验收：双容器创建/认证探活、幂等收敛、代理代次重建、bearer 轮换及独立停止通过；临时容器、网络和卷已清理。未用真实 OpenAI 凭据进行生产请求。
- 当前本机 HTTP/浏览器预览使用合成账号且全局 Docker 关闭，实际部署需 Compose 容器网络；本轮分别验证管理链路和真实 Host Docker adapter，未将完整 Compose 管理流程标为端到端验收完成。
- 当前边界：单实例 Docker 网关部署；quota/catalog/OAuth refresh 仍使用账号原有出口；删除需先停止并解绑，确认后清理 owned Docker 容器、独立网络和 HOME 数据卷；失败持久化重试。


## 最新实测：完整 Docker 网关与身份检查

- 独立 Linux 网关已从当前源码构建并启动，测试页面为本机 `http://127.0.0.1:18081/containers`；原生 `18080` 预览未替换。
- 已经通过真实管理员 API 完成空槽创建、代理配置、账号绑定、启动，再核对对应 Docker 容器；两个槽位的身份、网络和卷独立，代理切换触发重建并保留身份/HOME，停止 B 不影响 A。
- 测试使用合成 OpenAI 账号，A 绑定用户提供的真实代理，B 使用模拟代理作为隔离对照。真实代理密码只存在受限测试配置/数据库/容器文件，不得进入报告或提交。
- 从 A 的网络命名空间使用其挂载代理配置查询公网出口成功；访问 ChatGPT 的 sidecar 请求返回 502，独立 curl 经同一代理也遇到连接重置。未验证真实账号推理可用。
- **未实现：代理时区自动同步。** 代理所在地为有效 IANA 时区，容器实际 TZ 仍为 UTC；创建槽位代码固定 UTC，代理 location 尚未接入槽位身份。
- **未实现：槽位专属 TLS profile。** 两个 sidecar 的 ClientHello 配置特征摘要相同；独立 hostname/machine-id/installation ID/HOME 不等于独立浏览器或 TLS 指纹。
- Provider 仍使用账号凭据中的 installation ID，未统一到容器内槽位 installation ID。当前探活也不验证代理/账号可用性；页面的“运行中”不能解释为推理可用。
- 工作目录 `work/slot-inspection`（位于仓库父目录）保存测试脚本和脱敏结果，`outputs/container-inspection.md` 为完整实测报告。Linux 构建缓存可复用；为释放磁盘已清理本机 Rust 的生成依赖缓存，源码和运行网关二进制保留。
- 下一步应优先补齐时区跟随、请求身份一致性和运行/出口/账号状态区分；不能把本轮“检查完成”记成这些能力已经实现。

## 项目目标

在 `codex-proxy-rs` 上增加可选的“一账号一独立 Docker 卡槽”能力，复用原项目的账号选择、请求编码和响应解析，不复制 `vm2api`/`codexvm2api` 的实现。

目标链路：

```text
Gateway
  -> 选中 OpenAI/Codex 账号
  -> 该账号的 slot-sidecar 常驻容器
  -> 该账号绑定的 HTTP/SOCKS5 代理
  -> OpenAI
```

已确认的产品约束：

- 一个账号对应一个常驻容器、一个稳定 named volume、一个专属 Docker network。
- 每个启用槽位的账号必须配置独立 HTTP/SOCKS5 代理。
- 槽位异常必须 fail closed：立即退出调度，绝不回退网关直连。
- 全局 Docker 能力开关默认关闭；目标交互通过独立槽位启停，原账号级开关方案待上述流程替换。未绑定槽位的账号维持既有行为。
- 容器重建后保持 hostname、machine-id、installation ID、HOME、时区和客户端状态。
- 不模拟 TPM、SMBIOS 或其他物理硬件。
- secret、代理 URL、邮箱、原始账号 ID 和 bearer token 不得进入 Docker labels、普通日志或 Debug 输出。
- Docker 资源只允许按精确 owner label 操作，禁止广泛删除；孤儿槽位停止时保留账号 volume。
- 仅推理流量进入槽位；quota、catalog、OAuth refresh 等维护流量暂时沿用现有路径。

## 分支与提交

当前开发分支：

```text
codex/continue-account-slots
```

已完成提交：

```text
08861ca7 docs(slots): design isolated OpenAI account slots
32502957 feat(slots): persist OpenAI account slot intent
c99bd0c6 feat(slots): define host slot configuration
86e8dab6 feat(slots): add authenticated OpenAI slot sidecar
6d2d8539 feat(slots): reconcile account containers
```

设计与实施计划：

- `docs/plans/2026-09-22-openai-account-slots-design.md`
- `docs/plans/2026-09-22-openai-account-slots.md`

## 已完成

### 1. 数据库槽位意图

- 新增 `openai_account_slots` 迁移和冻结哈希。
- 新增稳定 slot instance UUID、generation、软件身份和槽位 store 端口。
- PostgreSQL 可读取账号槽位期望状态。

### 2. Host 配置

- 新增 `host.openai_slots`，默认关闭。
- 已包含 sidecar image、Docker endpoint、对账周期、启动超时、内存、CPU 和 PID 限制。

### 3. slot-sidecar

- 新增 Rust binary：`backend/apps/slot-sidecar`。
- 内部协议：`GET /readyz`、`POST /internal/v1/forward`。
- bearer token 和代理 URL 只从文件加载。
- 只允许 `/backend-api/codex/responses`。
- 强制使用账号代理，禁止 redirect，流式回传 SSE，过滤 hop-by-hop/internal headers。
- `/readyz` 也要求每槽 bearer token。

### 4. Docker 对账器

- 新增 Bollard Docker Engine adapter、运行时 registry 和 reconciler。
- 每槽创建稳定命名的 container、volume、network。
- Gateway 容器加入每个槽位 network，sidecar 不发布宿主端口。
- secret 和稳定身份用 tar 投影到容器，auth/proxy 文件权限为 `0600`。
- 容器有内存、CPU、PID、cap-drop 和 no-new-privileges 限制。
- 只有认证探活成功的 Ready 槽位才发布 route。
- image/generation 变化时精确重建容器并保留 volume。

已通过：

- `gateway-core` 聚焦测试与严格 Clippy。
- `gateway-host` 配置、对账和纯 Docker 资源单测与严格 Clippy。
- `codex-slot-sidecar` 代理转发测试与严格 Clippy。

最初交接环境未运行 Docker 集成测试，当前 Linux Docker 验证入口和结果见第 9 项。

## 当前开发进度

### 5. OpenAI Provider 槽位路由（本地修复完成）

基于 `505f78a3` 继续开发，读取和修改入口仍是 Provider、HTTP/SSE transport 和 sidecar。

已完成：

- 修复交接中的两个 transport 测试：检查真实转发 envelope；使用 connection-local previous response 构造必须 WebSocket 的续写。
- 修复真实协议路径不一致：sidecar envelope 使用 `/backend-api/codex/responses`，不再发送仅供网关拼接 base URL 的 `/codex/responses`。
- 内部 headers 使用 `(name, bytes)` 列表，保留同名多值和非 UTF-8 头；sidecar 保留 JSON 字段顺序与大数精度。网关与 sidecar 需使用同一源码版本构建。
- sidecar 自身错误携带受控的 `x-cpr-slot-error` 标记；上游同名标记被剥离。认证/校验失败映射为 `Unavailable + NotSent`，代理转发错误保守映射为 `Unavailable + Ambiguous`，不使账号凭据失效。
- 已启用槽位的图片和 standalone search 在发送前拒绝；当前 sidecar 只支持 Responses，其他推理端点不得绕过槽位直连。
- 移除 slot transport 的内联测试，相关断言通过 `tests/transport/slot.rs` 和 Provider contract 测试验证。

已通过的本地验证（macOS / Rust 1.97.0）：

- 两个 `transport::slot` 用例。
- 四个 `provider::contract::account_slots_*` 用例，覆盖目标隔离、缺路由/存储失败、sidecar 错误与不支持的推理端点。
- sidecar 三个集成用例，包含真实本地捕获代理、重复头、JSON 大数和错误标记过滤。
- `provider-openai`、`codex-slot-sidecar` 的 `clippy --all-targets --locked -- -D warnings`。

本地运行测试时使用 `NO_PROXY=localhost,127.0.0.1,::1` 和对应小写变量，避免默认客户端把本地模拟上游送入环境代理。
完整 Provider 回归首轮 722 通过、8 失败，集中于 WebSocket 时序与 macOS 原生 TLS 指纹；未将完整回归标记为通过。
已在原始 `505f78a3` 独立 worktree 运行全量基线：717 通过、9 失败，包含交接中的两个槽位失败与本次共同的七个失败。
当前另外一个 WebSocket 复用失败在串行重跑时通过；串行全量在原有并发连接上限用例停滞后终止，不记为通过。
Rustfmt 全工作区检查和 `git diff --check` 通过。
第 5 项验证时尚未启动 Docker Engine，真实容器验证在第 9 项继续。

### 6. Host/Store/Provider 组装（本地完成）

- StoreBundle 暴露既有 PostgreSQL slot store；Host 经 Core 端口组合槽位意图、账号启用状态及当前代理，保持依赖边界。
- 组合根创建共享 Ready registry，注入周期 reconciler 与 OpenAI Provider。旧 `initialize(config, ports)` 入口保留，新增 `initialize_with_account_slots` 接收可选槽位依赖。
- reconciler 作为 `AccountSlotReconciliation` 任务由既有 WorkerSupervisor 调度、退避和关闭，不自行 spawn，也不申请跨实例 Redis lease。
- 支持 `host.openai_slots.gateway_container` 显式容器 ID/名称，缺省读取 `HOSTNAME`；Docker 网络匹配兼容短 ID，避免每轮重复连接。
- 全局关闭时不读取容器环境、不创建 Docker client，贡献 Disabled worker；Provider 仍读取账号槽位意图，已启用账号保持 fail closed。全局关闭不会停止已存在的容器。
- 存储/Docker 枚举错误、取消、中止和 worker 退出均清除旧 Ready 路由；孤儿路由在等待容器停止前撤销。只发布实例和 generation 与期望一致的 Ready 路由。
- Docker 内联测试迁移到 `tests/slots/docker.rs`，通过本地 Unix socket 模拟 Docker API 验证实际容器配置、资源归属、秘密文件权限与短容器 ID；没有放宽生产源码禁用测试钩子的规则。
- 架构成员清单纳入现有 sidecar crate、Host slots 模块及 Core slot store 端口；架构文档和示例配置同步更新。

本轮验证（macOS / Rust 1.97.0）：

- 主程序 `cargo check` 通过；gateway、Host、Core、Store、OpenAI Provider 的严格 Clippy 全目标通过，最终 Host 变更单独复查通过。
- 主程序和架构测试 42/42、Core 任务契约 8/8、最终槽位及配置回归 16/16 通过。
- Host 首轮并行全量 100 通过、12 个系统更新用例超时；单例复跑及随后串行全量 112/112 通过。之后增加的孤儿路由用例已包含在最终 16 项回归中。
- Rustfmt 和 `git diff --check` 通过；真实 Docker、PostgreSQL/Redis 启动链路仍待集成环境验证。

### 7. 旧账号开关 API（历史实现，写入已被容器管理替代）

- 独立的批量查询与带 `expectedGeneration` 的槽位更新接口，统一管理员身份、错误信封和 no-store 策略
- Store 事务重新核对 OpenAI OAuth 类型与有效代理，保留实例与设备身份，写配置 revision 和管理员审计
- 新增 0018 迁移，在账号代理地址变化时推进 generation，覆盖单条、批量及代理管理入口
- Provider 只使用实例及 generation 匹配的 Ready 路由，管理端仅返回开关、代次和脱敏状态
- 全局关闭、账号停用、代理缺失与运行失败均有明确状态，旧页面不能覆盖新配置

### 8. 旧账号开关界面（历史实现，已移除开关）

- OpenAI 账号编辑框增加单独保存的独立槽位开关，未配置代理时禁止启用
- 账号列表增加可隐藏的独立槽位列，状态每 5 秒刷新，离开页面取消请求和定时器
- 复用 BaseSwitch、BaseTag 和现有主题，保存期间阻止关闭编辑框及并发保存账号
- 读取失败显式显示错误并禁止开关，不把未知状态显示成未启用

### 9. Docker 打包和集成验证（本地实现，最终验证中）

- `deploy/Dockerfile.slot-sidecar` 构建固定基础镜像的 sidecar
- `deploy/compose.slots.yaml` 显式开启槽位并挂载 Docker socket，主 Compose 默认不授予 Docker 控制权
- 配置支持 `CPR_OPENAI_SLOTS_ENABLED`、`CPR_OPENAI_SLOTS_IMAGE`，部署文档说明 socket 的宿主 root 权限边界
- `deploy/tests/verify-account-slots.py` 使用两个真实捕获代理，验证资源、身份、代理认证、转发、持久卷、日志脱敏及故障隔离
- `deploy/tests/verify-account-slot-host.py` 在 Linux 临时网关容器中验证实际 Host adapter 的健康检查、幂等收敛、代次重建和停止
- 所有脚本仅操作随机测试资源，测试用凭据为合成值，结束后清理本轮容器、网络与卷

## 关键验证命令

```powershell
cargo +1.97.0 fmt --all --manifest-path backend\Cargo.toml -- --check

cargo +1.97.0 test --manifest-path backend\Cargo.toml `
  -p gateway-host --test main slots --locked

cargo +1.97.0 test --manifest-path backend\Cargo.toml `
  -p gateway-host --lib --locked

cargo +1.97.0 test --manifest-path backend\Cargo.toml `
  -p codex-slot-sidecar --test main --locked

cargo +1.97.0 test --manifest-path backend\Cargo.toml `
  -p provider-openai --test main transport::slot --locked

cargo +1.97.0 clippy --manifest-path backend\Cargo.toml `
  -p gateway-host -p provider-openai --all-targets --locked -- -D warnings

git diff --check
```

Windows 上完整 `gateway-store` 编译仍会被仓库原有 Unix-only 代码阻断：

```text
backend/crates/gateway-store/src/backup/pg_dump.rs
tokio::fs::OpenOptions::mode(0o600)
```

这是既有问题，不要为本功能修改无关备份代码；在 Linux/CI 验证 store。

## 开发纪律

- 仅新增数据库迁移，不修改已经发布的迁移。
- 不读取、打印或提交真实账号凭据和代理密码。
- 新代码注释使用中文。
- Rust 变更必须通过 Rustfmt、聚焦测试和 `clippy -D warnings`。
- 槽位故障绝不能回退直连。
- 不把邮箱、原始账号 ID、token 或 proxy URL 写入 Docker resource name/label。
- 保留用户无关改动，不使用 `git reset --hard` 或其他破坏性命令。

## 最新变更：启停与真实删除

- 新增 0020 冻结迁移，删除先持久化意图；已停止且未绑定账号才能请求删除，清理期间禁止重新绑定和启动。
- Host 按明确删除意图清理精确 owner/instance 资源，先检查全部资源和网络端点；不强制断开外部容器，不强制删除占用卷。全部清理成功才完成数据库删除，部分失败保留意图并自动重试。
- `/containers` 新增删除按钮、前置条件说明和永久删除确认，展示 deleting/delete-failed；全局关闭时明确清理等待开启。
- Sidecar 处理 SIGTERM，并把长连接排空限制为 5 秒；普通和未完成请求的真实 Docker 停止均以退出码 0 完成。
- 18081 Linux 测试网关已更新，镜像 codex-slot-gateway:lifecycle / codex-slot-sidecar:lifecycle；18080 原生预览仍为旧后端。
- 新建临时槽位验收真实启停、HOME 保留、外部网络端点保护、占用卷清理失败与自动恢复、容器/网络/卷/数据库清理和空槽删除，均通过；已有槽位配置保留。
- 结果见父目录 outputs/container-lifecycle.md、container-lifecycle.json 和 vm2api-container-gap-review.md。

## 当前账号绑定界面

- 容器绑定候选显示未绑定、当前容器、占用容器名称；保留一账号一容器约束，已占用账号需先在原容器停止并解绑。
- 已绑定槽位清空代理后仍可解绑，无代理时继续禁止绑定新账号；页面支持刷新授权账号列表。
- 账号列表“绑定容器”列直接复用容器查询接口，显示名称和运行状态，明确区分未绑定、读取中与读取失败。
- 18081 前端已更新，`codex-slot-gateway:binding-ui` 与 `codex-slot-gateway:lifecycle` 指向包含本轮页面的镜像，sidecar 未改动。
- 前端 ESLint、类型检查和构建通过；浏览器实际验收未绑定账号选择、绑定名称、无代理解绑和全占用提示，临时夹具已清理，既有账号绑定保留。
- 本轮前端结果见父目录 outputs/oauth-container-binding.md 和 oauth-container-binding.json；未重跑 Rust 检查。
