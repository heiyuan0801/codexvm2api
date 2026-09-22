# OpenAI 账号独立槽位设计

## 目标

为 `codex-proxy-rs` 的 OpenAI 账号增加可选的 1:1 Docker 槽位。启用后，一个账号固定对应一个常驻容器、一个持久状态卷和一个独立网络出口；账号的上游 HTTP/SSE 请求必须经该容器和账号绑定代理发出。槽位不可用时账号退出调度，不回退到网关直连。

该能力默认关闭，现有账号和部署保持原行为。首版只支持 OpenAI Provider，不改变 xAI 路径。

## 边界

- 槽位是 VM-like Docker 容器，不是 KVM、VMware 或特权容器
- 稳定软件身份包括 hostname、machine-id、安装 ID、HOME 和时区
- 不模拟 SMBIOS、TPM、磁盘序列等物理硬件
- 不复制 `vm2api` 的预编译组件或实现源码
- 不使用实验性的 Codex App Server 作为 Responses API 转发器
- PostgreSQL 仍是账号凭证和期望配置的权威源
- 代理是槽位启用的硬性条件，缺少代理时槽位不进入 Ready

## 架构

`gateway-host` 新增槽位监督器，通过 Docker Engine API 对账容器、网络和卷。每个槽位运行仓库新增的 `codex-slot-sidecar`，只暴露内部健康检查和转发接口，不映射宿主机端口。网关和单个槽位共享一个专属 Docker 网络，其他槽位不能访问该 sidecar。

OpenAI Provider 保留账号选择、协议编码、额度与失败分类的所有权。在选中账号后，Transport 根据账号槽位模式选择现有直连路径或 Slot Transport。Slot Transport 把已经规范化的上游请求交给对应 sidecar；sidecar 使用账号绑定代理建立上游连接并流式回传状态、响应头和正文。网关与 sidecar 之间使用每槽随机密钥认证，密钥通过只读文件挂载，不出现在容器环境变量、labels 或日志中。

```text
Client -> Gateway API -> Core -> OpenAI Provider
                              -> selected direct account -> existing transport
                              -> selected slot account   -> private slot network
                                                          -> slot-sidecar
                                                          -> account proxy
                                                          -> OpenAI upstream
```

## 数据与状态

新增 `openai_account_slots` 表：

- `account_id`：引用 `provider_accounts`
- `enabled`：账号是否要求槽位
- `instance_id`：稳定且不可由账号邮箱推导的 UUID
- `identity_json`：稳定软件设备身份
- `desired_generation`：配置变更代次
- `created_at`、`updated_at`

运行健康是可恢复观测，不作为 PostgreSQL 业务事实。Host 在内存中维护 `Starting/Ready/Degraded/Stopped` 快照，并在进程启动后重新对账。容器通过 labels 记录所有权、instance ID、镜像版本和 generation，绝不记录账号凭证或代理 URL。

部署配置新增 `host.openai_slots`：全局开关、sidecar 镜像、Docker Socket、对账周期、启动超时、CPU/内存/PID 限制。默认关闭；开启后只有账号级开关为真且绑定代理的 OpenAI 账号才会创建槽位。

## 生命周期

监督器把数据库期望状态持续收敛为 Docker 实际状态：

1. 读取启用槽位的 OpenAI 账号及代理绑定
2. 为新账号创建持久卷、私有网络、认证文件和容器
3. 启动 sidecar，并同步稳定身份及代理配置
4. 通过认证健康检查确认 Ready
5. 发布健康快照，使 Provider 可以调度该账号
6. 凭证 revision、代理或 identity generation 变化时原子同步并重新探活
7. 账号禁用或槽位关闭时停止容器，但保留卷和稳定身份
8. 账号删除时删除容器和网络；数据卷由显式清理操作删除

监督器既响应配置提交，也运行周期对账，以处理网关重启、Docker 重启、容器被手工删除或镜像升级。

## 失败处理

- Docker 不可用：所有槽位账号不可调度，直连账号不受影响
- 缺少账号代理：槽位保持 Degraded，API 返回明确原因
- sidecar 启动或健康检查失败：指数退避重建，账号 fail closed
- 私有通道认证失败：不重试业务请求，槽位标记 Degraded
- 代理连接失败：沿用 Provider transport 错误分类和重试安全边界
- 客户端取消：取消 sidecar 请求并关闭上游流，不在后台继续生成
- 网关关闭：先停止接收新请求，等待在途请求，再停止监督器；槽位容器保持运行以缩短重启恢复时间

## 管理界面

账号编辑弹窗为 OpenAI 账号增加“独立槽位”开关。未绑定代理时开关不可启用，并就近显示原因。账号表以紧凑状态标签显示 `未启用/启动中/就绪/异常`，异常状态提供脱敏原因。全局功能关闭时，账号开关保留但显示“全局未启用”。

界面复用现有 Base 组件、主题 Token、请求封装和账号更新流程，不新增页面级颜色体系或独立布局。

## 验证

- 迁移与 Store 测试覆盖默认关闭、启用约束、账号删除级联和 generation 更新
- Host 单元测试使用假的 Docker Engine Port 验证期望状态收敛
- sidecar 测试使用本地捕获代理验证所有上游请求确实经过指定代理
- Provider 合同测试验证 Ready 才可调度以及故障时绝不直连
- 容器集成测试创建两个账号槽位，验证容器、卷、网络、身份和出口互不共享
- 前端执行 ESLint/类型检查/构建，并在浅色、深色和窄窗口验证账号编辑及状态显示

