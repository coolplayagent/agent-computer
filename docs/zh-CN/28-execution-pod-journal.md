# 28. 执行 Pod 身份持久化

## 28.1 已交付行为

迁移 15 为[执行派发日志](24-execution-dispatch.md)增加不可变 Pod 计划和 UID 观察记录。可信适配器必须在尝试创建之前登记已编译的 [Candidate 启动 Pod](27-candidate-pod-mounts.md)。计划将原派发意图与命名空间 UID、稳定 Pod 名称、完整清单和带领域前缀的 SHA-256 摘要绑定。清单含私有命令与存储信息，仅可信 Store API 可读取，没有租户 HTTP 路由。

`register_candidate_execution_pod` 接收原来的不可克隆派发凭据，检查固定执行/bootstrap/存储身份、当前凭据和授权、写入租约状态及原期限。计划与仅含元数据的 Outbox 事件原子提交。只有首次事务返回 `ExecutionPodAttempt`；并发或相同输入重试均返回 `DispatchAlreadyStarted`，包括提交响应丢失后的重试。新凭据沿用原单调时钟截止时间，不续租，也不启动命令。

Store 检查身份、清单大小上限和摘要一致性。完整 Pod 安全配置的编译与验证、真实部署/存储对象观察、实际准备文件系统复核仍由 Kubernetes 适配器负责。数据库接纳的模拟清单不能据此被视为可部署 Pod。

## 28.2 观察与启动绑定

可信适配器按固定计划验证真实 Pod 后调用 `record_candidate_execution_pod`。首次 UID 不可修改；相同 UID 重试幂等，不同 UID 或计划摘要冲突。观察记录与 Outbox 事件原子提交。取消或过期后仍可补记观察，因为它只提供恢复信息，不授予权限。创建响应丢失后不得重建替代 Pod。

存在计划时，[启动授权](25-execution-startup.md)必须使用其已记录 UID。Store 方法和 PostgreSQL 触发器均拒绝未记录或不同的 UID。启动授权已提交后不能补登记创建计划。未登记计划的派发保留迁移 14 的底层可信组件 API；它不是公开运行接口。迁移 15 保留历史授权，不凭空补造 Pod 计划或 UID 观察。

`candidate_execution_pod` 提供只读恢复快照并验证摘要与身份，不重新签发创建凭据或启动授权。读取或克隆快照不授予修改权限。后续适配器接线必须观察原名称/UID，清理时采用条件删除；对象缺失、Pod UID、进程报告或删除本身均不证明写入已排空。

## 28.3 验证与待办

新增 7 项 PostgreSQL 契约覆盖并发登记、响应丢失与 WAL 崩溃恢复、不可变 UID 选择、启动绑定、移植执行/bootstrap/存储身份拒绝、SQL 触发器保护、计划/观察 Outbox 回滚、撤权、过期和迁移兼容。输入与观察使用模拟适配器夹具，不创建 Kubernetes Pod。默认工作区共 285 项测试，其中 PostgreSQL 128 项。

按 [08 持久化](08-persistence.md)配置 PostgreSQL 环境并运行 Cargo、Bazel 测试；格式、Clippy、双语文档和现有完整 Qualitygate 策略另行验证。

本增量提供运行工作器需要的持久化边界。固定运行输入读取、数据库授权到实际 Pod 创建/attach 的连接、独立 watchdog、物理 fencing、输出对象及完成接纳仍待实现。未知执行结果继续保留 Draining 写入租约。T01–T43 仍为 `not_run`。
