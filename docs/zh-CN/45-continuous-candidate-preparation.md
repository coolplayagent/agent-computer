# 45. 常驻 Candidate 准备 worker

可信操作员现在可以在一个已验证的本地 Volume 上持续处理获准启动的 Computer，无需为每个请求单独执行准备命令。这个 worker 将持久化启动准入连接到 Candidate 存储准备；准备完成只证明存储事实，不代表 Computer Ready 或 App 健康。

## 操作员命令

```sh
agent-computer-server candidate-worker --database-url-file /private/control-url --organization example --worker-id preparer --config-file /private/candidate.json --concurrency 2 --poll-ms 1000
```

启动前先运行数据库迁移。私有配置沿用[原有 Candidate worker](17-candidate-preparation-worker.md)：已验证的 `target`、`mount_root`、私有 `object_cache`、固定 JuiceFS `quota` 命令配置，以及恢复已发布输入所需的可选 `artifacts` 对象存储（[38](38-workspace-artifact-checkpoints.md)）。Volume 必须已有持久化供应成功记录，并在本机挂载；此命令不负责供应或挂载 Volume。

轮询前会打开实际挂载和缓存、检查配额配置，并验证已配置的对象客户端。每项任务再次检查本地存储。文件系统配置操作放在线程任务中并等待完成，不通过超时丢弃未完成的文件系统操作后悄悄释放并发槽位。这些检查不能代替数据库中的当前授权、远程对象完整性校验和实际目录配额核对。

每个进程允许 1–4 项已调度操作，默认 1；轮询间隔 250–5000 毫秒，默认 250，每次最多调度一项。选项边界与[执行 worker](44-queued-execution-worker.md)共用，不是整个部署的容量限制。进程记录上次请求 ID，按 ID 顺序轮换可处理请求。失败、Busy 和存储结果未知的任务有五秒本地冷却期，因此撤权或无法恢复的请求不会独占队列；数据库错误按固定轮询间隔继续。

SIGTERM 和 Ctrl-C 停止新调度，并等待所有已调度操作，包括正在等待数据库领取的任务。停止期间可以取消只读发现查询。已调度任务仍可在信号后依据原授权领取、准备和记录结果，但不会获得更长的队列期限。强制杀死进程会丢失内存工作，持久化租约和身份仍保留，供后续协调。

[systemd 示例](../../deploy/systemd/agent-computer-candidate-worker.service)从 root 所有的配置文件读取 `DATABASE_URL_FILE`、`ORGANIZATION`、`WORKER_ID`、`CONFIG_FILE`，使用两个槽位、一秒轮询及 240 秒停止超时。启动前应准备好已验证的 JuiceFS 挂载。服务管理器的超时不证明物理 I/O 必然按时完成。逐行 JSON 日志报告调度、返回结果、未确认任务和停止，不含用户文件字节或凭据；接收端需要持续读取，同步输出阻塞会延迟调度。

## 发现与授权

发现查询只读，最多返回平台现有上限内的 64 个活动启动请求，限定精确组织和 Volume。请求必须仍对应 Computer 当前 generation，具有已提交 Workspace 输入，并处于原始十五分钟期限内的 Queued，或已记录派发的 Preparing。Prepared、Sealing、Sealed、Cancelled、Stopped 均排除。尚未过期的准备租约会被跳过；已有准备绑定必须逐字段匹配目标。迁移 25 为 Queued/Preparing 启动增加部分索引。

列表里的 ID 不是许可。原有原子领取仍检查原主体/凭据、固定依赖图的全部授权与目录依赖、Volume 成功证据、不可变输入、完整目标身份和协调 epoch。多个进程可以发现同一 ID，但只有一个取得当前租约，其他进程得到 Busy。权限失效会保留请求与资源预留，不伪造取消或资源释放。

尚未派发存储操作时，协调租约过期后可以在原始队列期限内重新领取执行。派发日志提交后，后续领取一律只能 Observe。恢复核对原目录发布、inode、文件系统身份和配额，可以补记真实发布成功但数据库确认丢失的回执。缺失或不确定的发布保持 `storage_unknown`，不会重建。原有 180 秒协调租约保持不变；超过租约的慢操作可能需要后续观察才能提交回执。轮询不会释放保留的资源预留。

## 验证与剩余工作

PostgreSQL 测试覆盖只读发现、组织/目标匹配、并发领取、活动租约、取消、WAL 重启、只观察恢复、当前授权及迁移保留。真实夹具同时调度一个新准备和一个已发布但丢失数据库确认的目录；它持有组织锁，在两项操作等待领取时发送 SIGTERM，然后释放锁，要求两份回执均提交。恢复目录必须保留原 inode 和内容。重启不再调度已完成请求，之后出现的缺失发布仍保持缺失和未知。

[固定源码的组件记录](../evidence/continuous-candidate-preparation-2026-10-10.json) · [验证日志](../evidence/continuous-candidate-preparation-2026-10-10.log)

Volume 编排、完整 Computer 生命周期调度、运行时/App 健康、自动排空恢复和产品验收尚未完成。Computer `ready=false`，公开 execution 仍不支持，T01–T43 保持 `not_run`。
