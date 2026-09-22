# OpenAI 独立账号槽位开发交接

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
- 全局开关默认关闭，账号级开关逐个启用；未启用账号维持原有直连行为。
- 容器重建后保持 hostname、machine-id、installation ID、HOME、时区和客户端状态。
- 不模拟 TPM、SMBIOS 或其他物理硬件。
- secret、代理 URL、邮箱、原始账号 ID 和 bearer token 不得进入 Docker labels、普通日志或 Debug 输出。
- Docker 资源只允许按精确 owner label 操作，禁止广泛删除；孤儿槽位停止时保留账号 volume。
- 仅推理流量进入槽位；quota、catalog、OAuth refresh 等维护流量暂时沿用现有路径。

## 分支与提交

当前开发分支：

```text
codex/openai-account-slots
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

当前开发机没有 Docker Engine/WSL，所以尚未运行真实 Docker 集成测试。

## 当前未完成的 WIP

### 5. OpenAI Provider 槽位路由（开发中）

当前工作区包含尚未完成的 WIP：

- `backend/crates/providers/openai/src/transport/slot.rs`
- `backend/crates/providers/openai/tests/transport/slot.rs`
- Provider、HTTP/SSE client 和 failure mapping 的相关修改。

已有实现方向：

- Provider 选定账号后读取账号槽位意图。
- 未启用槽位的账号维持原直连。
- 启用槽位但 runtime 没有 Ready route 时返回 `Unavailable + NotSent`，禁止直连回退。
- Ready 槽位把规范化 headers/body 封装进内部 JSON 请求，外层 bearer 只用于 sidecar 认证。
- 槽位暂只支持 HTTP/SSE；WebSocket-only continuation 必须发送前失败。

当前状态：

- Provider library 单测能编译通过。
- Provider 严格 Clippy 能通过。
- 新增的两个 `transport::slot` 集成测试仍失败，尚未提交正式 feature commit：
  - `ready_slot_is_the_only_http_destination` 的 wiremock body 匹配写得过严，实际请求因 matcher 未命中得到 404。建议先只匹配 method/path/outer bearer，再读取 `received_requests()` 对 envelope 的关键字段逐项断言。
  - `websocket_only_request_fails_before_any_send` 只设置 `use_websocket=true`，这在现有协议里可能只是偏好而非强制。应按 `transport_requirement()` 的规则构造真正的 exact WebSocket continuation 请求，再断言 `SlotProtocol`。

继续前先运行：

```powershell
cargo +1.97.0 test --manifest-path backend\Cargo.toml `
  -p provider-openai --test main transport::slot --locked
```

修完后还需要补 Provider contract 层测试，证明：

1. 普通账号只请求原上游。
2. Ready 槽位账号只请求 sidecar。
3. enabled 但 unhealthy/route missing 的槽位在发送前 fail closed。
4. 槽位失败不会静默改走直连，也不会误判为账号凭据失效。

### 6. Host/Store/Provider 组装（未开始）

仍需完成：

- 用 PostgreSQL slot store + account store 实现 `AccountSlotDesiredStateSource`。
- 在 `gateway-host` 启动周期性 reconciler worker，并正确响应 cancellation。
- 从容器 `HOSTNAME`（或明确配置）取得 Gateway container ID。
- 全局关闭时不创建 Docker client；开启时初始化 registry/engine/reconciler。
- 给 `provider_openai::initialize` 增加兼容的可选槽位依赖，避免破坏现有测试和 XAI Provider。
- 把同一个 `AccountSlotRegistry` 注入 Host reconciler 与 OpenAI Provider。

### 7. Admin API（未开始）

- 增加启用/关闭账号槽位 mutation。
- 只允许 OpenAI 账号启用。
- 启用前必须存在有效账号代理。
- 返回脱敏的 desired/runtime 状态，不返回 token、proxy URL 或内部 endpoint。
- 配置提交后触发/等待下一轮 reconcile。

### 8. Vue 管理界面（未开始）

- OpenAI 账号编辑框增加“独立槽位”开关。
- 没有账号代理时禁用并给出原因。
- 列表显示 disabled/starting/ready/degraded/global-disabled。
- 复用现有 Base 组件和主题 token。

### 9. Docker 打包和集成验证（未开始）

- 新增 sidecar image 构建。
- Gateway 挂载 Docker socket；文档明确这是 root-equivalent 权限边界。
- Compose 配置全局开关和 sidecar image。
- Linux 集成脚本用两个捕获代理证明两个账号拥有不同 container/network/volume/identity/egress。
- 检查 logs、Docker inspect 和 labels 不泄漏代理凭据或 bearer token。

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
