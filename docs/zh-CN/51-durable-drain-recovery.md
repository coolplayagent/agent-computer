# 51. 已完成执行排空的持久恢复

原节点现在会在向 worker 返回之前持久记录已经完成的进程和 IO 封闭证明。若 worker 在数据库事务提交前退出，`execution-worker` 可以在原执行窗口关闭后补交保留的回执。恢复不会重复命令、Pod 创建、启动授权或续约。

## 回执与权限

只有原 `SealedExecution` 仍持有固定进程描述符和已封闭 Candidate IO 入口时，节点才会写入回执。内容沿用既有完成事务接受的 version 1 seal。先同步私有临时文件，再通过禁止覆盖的重命名发布，最后同步父目录。文件名绑定完整的原 watchdog 布防记录，内容另有摘要；冲突的第二份回执被拒绝。

节点在布防前固定配置中的 root 所有私有 spool。恢复检查全部路径祖先、文件类型、所有者、私有权限、单一硬链接、字节上限、摘要和数据库登记的确切布防记录。符号链接、未完成文件和移植回执不能证明排空。文件包含受信运行时元数据，不包含 stdout/stderr 或用户文件正文。需随原 watchdog spool 保留，尚无自动回执回收。

读取结果是不可伪造构造的历史排空回执，不是活 guard 或可续约派发句柄。普通 SQL/JSON 元数据、Pod 缺失、空 cgroup、watchdog 到期报告和恢复后的输出都不能代替它。旧安装没有这类回执时仍保持原恢复行为。

## 自动与显式恢复

队列在普通派发与符合条件的完成恢复之间轮换，共用既有 1–4 个任务槽及退出等待。只读发现每页最多返回 64 个 ID，限定确切组织、存储目标和原节点名称/UID。已完成且持久化的证明是历史事实，因此可以来自同一节点的旧 boot。游标越过缺失回执的项，并在最后一页后回绕。发现不授予权限，也不会把旧派发重新选为执行任务。

发现及发布事务均等待原截止时间和全部已签发续约时间到期，包括未确认的续约，从而为仍存活的原 worker 保留完整结果发布窗口。每次发布重新检查原布防记录、Candidate、派发和写租约 epoch；完成行、执行状态、写入排空和事件仍在同一事务原子提交。并发恢复返回同一不可变回执，历史重试不能释放后续写租约代次。

运维也可执行：

```sh
agent-computer-server execution-completion-recover --database-url-file /private/control-url --organization example --execution-id exec_example --config-file /private/execution-worker.json
```

该命令使用既有私有 worker 配置，但不读取 Kubernetes 凭据或调用其 API。证据缺失时返回 `completion: null`；证据无效或窗口尚未到期时失败，不发布完成记录。Pod 观察与删除仍由独立清理路径负责；补交历史回执不调用 Kubernetes。常驻 worker 分别输出 `completion_recovered`、`completion_recovery_failed` 和统计计数。阻塞的本地文件读取持续占用任务槽，可能延迟退出；超时不能证明 IO 已停止。

## 结果与未完成恢复范围

仍处于 `CancelRequested` 的执行可以凭原已完成排空变为 `Cancelled`，从而解除已准入检查点停止及 Artifact worker 的阻塞。取消仅确认写入停止，不撤销先前副作用。恢复的 `Dispatching` 或 `Unknown` 执行保持 `Unknown`，即使持久输出报告成功也不改判。物理写入者可以释放，但成果捕获仍拒绝包含未知执行的历史。

若崩溃发生在完成回执持久发布之前，排空仍未确认。本路径不重建丢失的进程/IO 句柄，也不提供跨节点 fencing、断电认证、任意 App/profile 检查点或外部副作用对账。完整生命周期调度、Browser/ComputerView 和产品验收仍待完成；Computer `ready=false`，公共执行支持保持原状。

## 验证

节点测试覆盖不可变发布、并发相同写入、固定目录、未完成/损坏/移植回执、权限、链接、FIFO 和字节上限。PostgreSQL 测试覆盖截止时间、组织/节点/存储范围、游标、只读发现、WAL 重启及已完成项排除。一次性 VM fixture 在真实 gVisor 控制器完成封闭后、数据库提交前强制终止它，验证缺失/损坏证据被拒、队列重启补交、取消后的检查点恢复，以及未知结果和缺失证明仍保留 Unknown。最终[源码绑定证据](../evidence/durable-drain-recovery-2026-10-11.json)固定了 376 个源码/构建/schema 文件与 9 个已安装二进制。完整 VM fixture 在 680 秒内通过 35 个执行场景。独立 SQL、SigV4 S3 和冷 JuiceFS 读取器校验了 271 个流式块、64 个最终输出对象引用、13 个 Candidate 文件，以及 3 份检查点中的 6 个恢复文件。两条补交完成记录各只有一次 dispatch、启动授权、完成与排空；未封闭场景没有持久回执、完成或排空记录。三个已终止 Pod 留待独立 API 清理，随后本次 VM 及私有磁盘、配置已一并回收。

Cargo 工作区通过 478 项测试，另补测通过新增的待确认续租测试；15 个 Bazel 目标全部通过，其中 PostgreSQL 合约测试 230 项。Clippy 将警告视为错误的检查、VM 内 18 项节点测试和 15 项 watchdog 测试均通过。证据保留早期 fixture/构建失败和一次原因未完整留存的启动拒绝，最终成功运行绑定所记录的源码与二进制摘要。细节采用有字节上限的 JSON 分片，可用 `docs/evidence/verify_fragments.py` 验证。完整原始运行记录及摘要保留于本地验证目录，提交记录保留新增恢复场景及独立核验结果。
