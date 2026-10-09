# 06. 领域状态契约

## 06.1 已实现规则

`crates/core` 实现领域内存值及纯状态变更规则。它不执行 I/O，不是持久化服务或单机替代运行时。

| 模块 | 已实现约束 | 规格 |
| --- | --- | --- |
| `identity` | 不同 ID 类型，独立 revision/generation/lease epoch，溢出拒绝，SHA-256 摘要格式 | D02 |
| `computer` | 稳定 Computer/Workspace/组织身份，启动代次，健康回执，活动保护，排空/恢复/墓碑 | D02、D04 |
| `lease` | 单拥有者状态，30 秒最大租期，到期拒绝动作，排空确认后换代，组织与连接绑定 | D06 |
| `execution` | 派发、实际结果、取消请求、Unknown 原事实及单独对账结果 | D08、D11 |
| `idempotency` | 组织/主体/操作/键的作用域，相同输入复用 execution，不同输入冲突，已回收键墓碑 | D08 |

ID 值当前限定 1–128 个 ASCII 字母、数字、下划线或连字符；摘要接受 `sha256:` 与 64 位十六进制。领域类型不负责生成全局唯一 ID、计算摘要或验证实际对象内容。未来传输 schema 必须明确其格式约束。

## 06.2 可信边界

所有修改必须由服务层鉴权，并在数据库事务中锁定/校验状态，原子保存 revision、幂等记录、事件和 Outbox。Rust 的 `&mut` 只能保证单对象本地修改，不证明跨进程互斥。数据库必须为每个 Computer generation 和 Lease scope 保持唯一权威记录，数据库时间用于租约判断。

`RuntimeFence`、`DrainEvidence`、`Receipt` 是可信适配器提供的已验证证据引用，禁止从客户端直接反序列化后信任。构造值不证明旧进程真的退出、按键已释放、隔离已完成或结果已持久写入。适配器还须验证证据的组织、对象、代次、输入、来源与存储完成状态。

普通 stop 保护人/Presentation 的活动，同时允许进入 Draining 等待执行。Idle stop 要求全部活动类别清空。Force stop 仍要求实际 fencing，并用明确损失记录表达未保存状态；最近有效 checkpoint 保留。

Unknown 禁止重新派发或接受普通迟到 finish。显式授权的 `reconcile` 只追加一次解决记录，`status` 仍保留 Unknown，读取者同时呈现 `resolution`。取消请求不把 Running 直接改为 Cancelled，完成与取消竞争时采用实际持久回执。

## 06.3 验证

`bazel test //crates/core:core_contracts_test` 覆盖阶段 02 的领域约束，包括拒绝后状态不变。测试来自设计故障条件；T01–T43 的外部运行验收仍未执行。后续服务测试须增加并发事务、崩溃恢复、伪造主体/证据与真实 worker 故障。
