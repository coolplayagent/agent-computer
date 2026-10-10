# 42. 执行完成与写租约释放

可信单次执行 worker 可以完成经过隔离的执行并释放 Candidate 写租约。它组合原始派发句柄、活的内核进程封闭证明、Candidate I/O 屏障、已验证的持久输出和 PostgreSQL 事务。公开执行能力仍不支持；Computer `ready` 保持 false，T01–T43 保持 `not_run`。

## 活的完成权限

发出启动授权前，节点适配器固定准入时的 cgroup 域，并为观察到的 gVisor sentry、gofer 打开 pidfd。每个进程在 `pidfd_open` 前后必须保持原始启动 tick 和精确 cgroup 归属；持有的 `/proc/<pid>` 目录防止数字 PID 复用替换身份。这些是私有活句柄，不是可序列化权限。

attach 尝试返回后，控制器关闭 Candidate 修改入口、撤销 CSI 发布并终止原始固定域。它同时要求 cgroup 子树为空、所有固定运行时进程均已退出。停止状态不等于退出。原域删除只能通过此前验证并固定的 cgroup v2 核心 `cgroup.events` FD 返回 `ENODEV` 确认；路径不存在、目录链接计数均不构成删除证据。实测 Linux 内核在目录删除后仍保留链接计数 2。

这一判断依赖内核的 [cgroup 删除顺序](https://github.com/torvalds/linux/blob/v6.8/kernel/cgroup/cgroup.c)：拒绝非空域及活的子域，禁止新迁移，再移除核心文件。[kernfs 读取路径](https://github.com/torvalds/linux/blob/v6.8/fs/kernfs/file.c) 对失活节点返回 `ENODEV`，其他读取错误仍不能确认退出。[cgroup.kill](https://docs.kernel.org/admin-guide/cgroup-v2.html) 覆盖后代和并发 fork；不使用 `PIDFD_THREAD` 的 [pidfd 轮询](https://man7.org/linux/man-pages/man2/pidfd_open.2.html) 观察进程/线程组退出，并在进程被回收后继续有效。这些观察本身不证明文件 I/O 已排空。

随后 worker 等待活 FUSE 修改屏障并同步 Candidate 根目录。生成的 `SealedExecution` 保留原节点 guard、固定内核身份和 `SealedFence`。证据可以序列化用于审计，句柄不能克隆或反序列化。独立 watchdog 继续保持原始期限，其固定 FD 不会把终止动作转向同名路径的新域。

## 持久结果与写租约

迁移 23 增加不可变 `execution_completions`。同一事务将封闭证明绑定到精确的派发、布防记录、Pod 计划、挂载实例、准备收据和写 epoch，记录接受的结果与 outbox 事件，追加 `execution_drained` 并释放写租约。延迟约束拒绝不完整事务，以及提交时已经跨过原派发期限的成功/失败结果。

| 完成时的状态与输入 | 接受的状态 |
| --- | --- |
| Dispatching、权限有效、原预算未过期、已验证 succeeded 输出 | Succeeded |
| 同上，输出为 failed、spawn_failed、timed_out 或 descendants_terminated | Failed |
| 显式 CancelRequested，且有活的进程与 I/O 封闭证明 | Cancelled |
| 输出缺失、权限或预算失效、监督器结果不确定，但有封闭证明 | Unknown |
| 已经 Unknown，且有封闭证明 | 保留 Unknown 及原 revision/reason |
| 缺少进程/I/O 证明或数据库不可用 | 不产生接受的完成，恢复保持不确定性 |

Cancelled 表示显式取消后的执行写入已经封闭，不会回滚先前副作用。Unknown 可以释放物理写入者，但结果仍未解决。持久输出恢复不能改写完成结果、重建活证明或单独释放租约。Artifact/checkpoint 发布继续拒绝包含 Unknown 的执行历史，即使物理写入者已释放。

同一原始派发与封闭句柄可以幂等重试。有限重试处理事务回滚或提交响应丢失，不重复命令、启动授权或 Pod 创建。已记录的完成先于当前写 epoch 检查，重试不会改动后续写入者。恢复从不可变 epoch 历史读取准备输入，租约进入新 epoch 后，旧执行仍能取得原清理输入。

## 恢复与边界

运维派发结果包含可选 `completion` 收据。现有执行 API 暴露 Succeeded/Failed，通过 `dispatch_started` 区分排队取消和已派发后完成封闭的取消。序列化输出、完成、watchdog 与 I/O 元数据都不授予执行或存储权限。

数据库操作有界结束后仍执行条件 Pod 清理，包括完成失败的路径。存活控制器可以封闭并完成；被杀死的控制器丢失活 I/O 句柄，替代控制器仅撤销发布、条件删除原 Pod 并读取不可变记录，不能从空 cgroup、缺失 Pod 或恢复输出重建封闭证明。跨节点 fencing、控制器丢失后的排空恢复、通用 App checkpoint 和 registry 自动回收仍待实现。

## 验证

默认测试覆盖 SQL 绑定拒绝、原子释放、显式取消、已验证输出、不可变及 WAL 恢复收据、历史 epoch 查询、Unknown 保留和提交期间期限失效。一次性 VM 测试还覆盖固定域删除/路径复用、停止的后代、真实 CSI/gVisor 执行、非零退出、超时、权限撤销、已有 Unknown、输出存储/发布失败、完成事务重试，以及下一写 epoch 的文件保存。仅在显式一次性 root VM 中，按[第 41 节](41-fenced-execution-csi.md)的私有配置运行手动测试。

证据通过独立 SQL 查询、签名 S3 读取及新的只读 JuiceFS 客户端收集。组件证据不代表完整 Computer 产品验收。

[组件记录](../evidence/accepted-execution-completion-2026-10-10.json) · [日志](../evidence/accepted-execution-completion-2026-10-10.log)
