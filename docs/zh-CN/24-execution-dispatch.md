# 24. 持久化执行派发日志

## 24.1 已交付行为

迁移 13 将 [23 执行队列](23-execution-admission.md) 接到独占写入派发日志。可信 Rust 存储客户端可以提交一次固定派发意图、在响应丢失后读取恢复输入、请求派发后的取消，以及记录不确定结果。事务重新检查原连接凭据和当前权限。这些方法不会创建 Kubernetes Pod 或启动进程；服务仍报告 `execution: unsupported` 和 `execution.admission: connection-queued`。

| 状态 | 含义 | 写入权交接 |
| --- | --- | --- |
| Queued | 已保留输入，尚无派发意图 | 取消可以释放排队预留 |
| Cancelled | 未派发的预留已取消 | 适用既有零派发或文件规则 |
| Dispatching | 派发意图已提交，外部效果可能存在或不存在 | 禁止交接 |
| CancelRequested | 派发后的取消请求已提交 | Draining，禁止交接 |
| Unknown | 无法确认派发结果或继续执行的权限 | Draining，禁止交接 |

`dispatch_started: true` 表示持久派发意图已提交，不证明进程启动、成功完成、终止或存储排空。调用者提供的状态不会生成 Succeeded、Failed 或 Running 结果。

## 24.2 事务与恢复契约

`begin_candidate_execution_dispatch(organization, execution_id, expected_revision)` 是可信 worker 方法，不是 HTTP 身份认证入口。它从已准入的连接派生 principal/credential。凭据和主体共享锁使撤销操作按序执行；connect/read/modify scope、活动连接、Computer/Workspace 的五项独立授权、当前 generation/Candidate、准备摘要与固定目录引用均须继续有效。协作者使用自己的连接权限，不依赖原启动凭据继续有效。

事务消费排队预留，插入不可变派发意图及匹配的写入日志，递增执行和写入租约版本，写入事件/Outbox。固定 execution ID 同时作为派发身份。摘要绑定组织、执行、租约/epoch、不可变输入和绑定摘要、开始时间与期限。SQL 延迟检查拒绝只写了一半的意图或日志；不可变触发器拒绝替换、删除、回到 Queued，或将已派发执行当作未派发取消。事件写入后再次检查权限和期限；失败会回滚全部变化。

只有成功提交的调用返回 `ExecutionDispatchAttempt`。这个 Rust 值不能克隆或反序列化；调用者持有期间，其本地单调时钟预算持续减少。重复 begin 一律返回 `DispatchAlreadyStarted`，包括返回值丢弃、进程崩溃或确认丢失。`candidate_execution_dispatch` 只读取固定输入和当前元数据，不签发第二次尝试。恢复数据属于可信内部接口，含命令字节和存储绑定；HTTP 元数据与事件不暴露这些值。

绝对期限仍是准入时捕获的队列期限。写入租约续租、恢复和重试均不能延长它。本地预算计入等待数据库事务的时间，但它只是适配器输入，不构成运行时强制边界：后续适配器必须在实际启动进程时重新验证，并从工作负载外部执行绝对到期约束。调度延迟的 Pod 不能重新获得一段完整相对预算。

## 24.3 取消、到期与不确定性

既有取消端点保留执行版本 CAS 和幂等语义。派发前返回 Cancelled；派发后返回 CancelRequested，并原子地把写入租约置为 Draining，不生成排空证明。重复请求返回当前元数据。已经 Unknown 的执行保持 Unknown，取消回执不能确定其结果。

固定期限到期、连接关闭、授权或凭据失效、Candidate/目录绑定不再有效时，元数据查询或可信协调会把已派发执行改为 Unknown。可信 worker 也可在外部操作无法确认后调用 `mark_candidate_execution_unknown`。重复不确定性报告不会重复发事件。事件提交失败时，不确定性、取消和写入租约 Draining 的变更一起回滚。

上述路径都不释放写入权。执行意图阻止第二次派发、下一写入 epoch、零派发证明，以及有界文件完成或排空凭据。恢复授权不能让意图复活。必须另有真实运行时协调证据，才能证明排空并交接。

## 24.4 验证与剩余工作

新增 11 项真实 PostgreSQL 测试，覆盖并发 begin/cancel、单次派发、WAL 恢复、精确重试、身份不可变、半事务拒绝、当前授权与协作者身份、续租后的固定到期、Outbox/最终到期回滚、取消回滚、禁止文件凭据及交接、迁移 13。新增 1 项 HTTP 测试，覆盖 Dispatching → CancelRequested → Unknown、版本冲突和凭据隔离。fixture 使用合成 Candidate 准备回执，不启动进程。

默认工作区共 254 项测试：115 项 PostgreSQL、20 项服务，以及原有 119 项其他测试。Cargo test、fmt/Clippy、Bazel build/test、OpenAPI 与中英文文档检查通过。完整 Qualitygate 只执行既有换行策略。T01–T43 仍全部为 `not_run`。

仍须实现实际 Candidate Pod/worker、可信监督器交付、存储挂载身份核验、进程启动授权、外部 watchdog 与物理 fencing、有界输出对象、可信完成接纳和独立后台存续期。[22 监督器暂停故障](22-sandbox-supervisor.md) 不能由本地监督报告或 Kubernetes 状态解决。在取得真实运行时证据前，本日志保留不确定写入权。
