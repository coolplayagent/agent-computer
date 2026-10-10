# 50. 持久执行输出分块

新请求默认启用 `stream_output`。受信任监督器在命令运行期间发布已保留的 stdout/stderr，调用方可轮询已验证序号、下载二进制分块，并在断连后继续读取。继续读取不会再次派发命令。范围仍是有界 Candidate 执行链路，公开 Computer 执行能力尚未开放。

## 策略与通道

省略字段的输入仍保持原来的规范化字节。只有新准入会把 `output_stream: {version: 1}` 加入不可变 binding；迁移 30 不为历史请求开启输出流，历史排队请求也不例外。显式 false 使用原 v1 固定期限或 v2 可续期协议。省略、true、false 是不同的重试输入。响应中的 `stream_output` 表示准入策略。

v3 为 bootstrap、challenge 和启动 grant 使用独立摘要域。输出流与续租互相独立：不含硬预算的 v3 使用原固定期限，含硬预算的 v3 使用已有双 guard 续租协议。v1/v2 字节和摘要保持兼容。

每块绑定启动 grant 摘要、共享序号、前块摘要、流名称、该流偏移、观察字节数、截断和 EOF。载荷至多 8192 字节，序列化帧至多 40 KiB，每次执行至多 8192 块。每条流最多保留准入上限内的字节（至多 1 MiB），超出内容继续排空并计数。完整块及时发送，小块约每秒发送，EOF 关闭各自的流。达到上限后不会无限发送空进度帧。

监督器采用非阻塞 IO，同时只保留一个待发送帧。续租挑战在帧之间优先，期限继续使用发送前的锚点。attach 验证身份、序号、各流偏移、摘要和 EOF 后才产生不可伪造的观察对象；读取取消会保留部分帧。合并传输的输出与续租帧各自维持独立摘要链。最终报告必须匹配全部已交付字节和准确进度，成功要求两条流均具有完整 EOF 前缀。即使命令已退出且输出到达 EOF，只要存在未完成的续租挑战，也必须收到相应授权后才能正常提交最终报告；等待不会延长已有期限。

## 持久化与中断

worker 使用单个顺序发布器，最多排队八块、发布一块。各发布器共享四个私有 spool 阻塞 IO 槽，异步任务取消不会提前释放仍在执行的阻塞工作槽。队列满时暂停 attach 读取，权限和期限检查继续。存储压力可能中断执行，不能延长租约。监督器可能先退出，其缓冲的最终报告才到达 worker。如果此时存活检查失败，只有原始固定 cgroup 和全部原始运行时 pidfd 都证明进程已退出，worker 才可继续接收报告；同时立即关闭 Candidate IO，停止续租，并保留原有权限与期限检查。接受完成仍要求后续的实时 IO 封闭证明。

分块首先提交不可变数据库意图及 Outbox 事件，绑定原派发、Pod、启动授权和 watchdog 布防。字节写入私有 spool 和固定内容寻址对象，通过签名请求完整读回验证后，确认记录与 Outbox 原子提交。PostgreSQL 只保存引用和计数，不保存 stdout/stderr 字节。调用方只能读取连续已确认前缀；该流 EOF 或最终报告之后不能追加分块。

`execution-output-chunks-recover --database-url-file PATH --organization ID --execution-id ID --config-file PATH` 使用原 manifest、spool 和对象身份重试唯一待发布块，返回 `recovered_chunk` 或 null。不需要 Kubernetes 凭据，也不重建 attach、启动、续租或完成执行。如果固定对象和原 spool 都没有待发布字节，恢复保持不可用。控制器退出可能丢失尚未记录的缓冲分块，但已验证分块仍可读取。可读前缀、EOF、已验证最终报告均不等于权威执行成功。

## HTTP 读取

- `GET /v1alpha1/executions/{id}/output-chunks?after_sequence=0&limit=16` 每次返回至多 32 块元数据。`next_sequence` 是续读游标，`available_sequence` 表示已验证前缀；`execution_state`、`final_sequence` 和 `final_report_verified` 分别描述执行与最终报告。空页不代表结束。元数据接口不依赖输出网关配置。
- `GET /v1alpha1/executions/{id}/output-chunks/{sequence}` 返回一个已验证二进制块，允许零字节 EOF 块。响应头绑定序号、流、偏移、SHA-256 和块摘要。分块及最终输出下载共享四个槽，响应体被读取或丢弃后才释放。
- 原 `/output/{stdout|stderr}` 返回最终保留内容，不提供运行期游标。

读取要求原有效凭据具备 `runtime.connect`、`runtime.read`，以及当前 Computer connect/read 和原 Workspace read 授权。对象 IO 前后均复核权限，包括 GET 失败时。逻辑断连或失去 modify 权限不会取消历史读取权限。每次重新验证对象大小、摘要及通道块摘要，不暴露对象 URL、key、监督器报告或凭据。未知、重复、空查询参数、越界值与 Range 均拒绝。元数据和字节响应设置 no-store/nosniff。

## 验证范围

契约夹具覆盖历史输入/序列化、独立续租策略、分片与合并帧、输出洪泛下的有界发送、不可变 SQL 摘要链、最终报告一致性、WAL 恢复、确认回滚、损坏和下载期间撤权。临时 gVisor 夹具覆盖 Dispatching 期间的早期 HTTP 读取、断连后运行 38 秒、固定期限输出流、双流保留上限、取消、控制器退出、S3 凭据拒绝及确认事务失败。验证证据必须绑定实际使用的源码和二进制。手动测试二进制还提供单独忽略的 `real_streaming_execution_outputs_and_publication_recovery`，以 `--ignored --exact` 运行七类聚焦场景；完整回归另行运行。

最新[持久化输出流增量](50-durable-execution-output.md)通过 471 项 Cargo 测试、15 个 Bazel 目标、fmt/Clippy、32 个真实执行场景、7 个聚焦流式场景、8 类节点续租故障以及 Candidate/CSI 回归。独立读取核对了 271 个输出块对象、60 个最终输出对象和 13 个冷读取 Candidate 输出文件。[源码绑定记录](../evidence/durable-execution-output-stream-2026-10-10.json)保留洪泛失败尝试、EOF/续租回归、进程退出后的报告接收诊断、准确二进制和 VM 清理证明。T01–T43 仍为 `not_run`。

详细证据保存在由摘要绑定的[流式轨迹](../evidence/durable-execution-output-stream-2026-10-10.cases.json)、[聚焦轨迹](../evidence/durable-execution-output-stream-2026-10-10.focused.json)、[输出块日志](../evidence/durable-execution-output-stream-2026-10-10.chunks.json)和[执行日志](../evidence/durable-execution-output-stream-2026-10-10.journal.json)中。主记录给出无损重组方法及规范化 SHA-256。

自动排空恢复、多节点 fencing、完整 Computer 生命周期调度、浏览器、ComputerView 和产品验收仍待实现。Computer `ready=false`，公开 `execution` 仍不支持，T01–T43 保持 `not_run`。
