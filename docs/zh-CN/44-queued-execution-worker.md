# 44. 常驻执行队列 worker

[48 有界后台执行](48-background-execution.md)增加默认后台 lifetime，仍保留原固定期限与身份要求。

可信操作员可以为一个组织和一个已验证的本地 Volume 启动常驻 worker。它自动领取排队的 Candidate 执行请求，并运行已有的受保护执行、输出发布、完成确认和清理流程。调用者仍通过持久化执行准入 API 提交，不选择节点，也不提供 worker 配置。

## 启动与停止

```sh
agent-computer-server execution-worker --database-url-file /private/control-url --organization example --config-file /private/execution.json --concurrency 2 --poll-ms 250
```

私有配置沿用[单次执行 worker](29-execution-worker.md)，包含 [31](31-node-guarded-startup.md) 的节点绑定，以及 [36](36-durable-execution-outputs.md) 的输出存储和 spool。配置固定 Kubernetes namespace UID、Volume ID、PVC/PV UID、文件系统 UUID、Volume 路径和写入 UID/GID。节点 boot ID 与可执行文件散列必须对应当前已验证的宿主机；节点重启或替换二进制后需要更新操作员配置。凭据在进程启动时加载，轮换后需要重启。

领取前会检查本地 root 身份、boot ID、固定可执行文件、私有 spool、运行时 socket 父目录及存储挂载配置。这些检查不证明 Kubernetes、S3、CSI 或 reaper 当前可用；每次执行仍重新检查实际运行时身份和授权。配置失败会在领取前退出。启动前先执行 `agent-computer-server migrate --database-url-file /private/control-url`。

每个进程允许 1–4 个活动任务，默认 1；轮询间隔为 250–5000 毫秒，默认 250，每次最多领取一个请求。这是进程级限制，不是整个部署的容量或公平性保证。已有准入上限和写入者互斥仍然有效。任务满额时停止领取，槽位释放后继续；轮询失败按固定间隔继续并报告事件。

SIGTERM 或 Ctrl-C 会停止新领取并等待已领取任务完成。正在进行的领取事务可以结束并进入等待集合。操作员停止 worker 不会直接取消任务；执行期限、调用者取消、当前权限检查和独立节点 watchdog 继续生效。强制杀死进程可能丢失活的完成证明，留下 Unknown/Draining，与单次派发相同。

[systemd 示例](../../deploy/systemd/agent-computer-execution-worker.service)依赖 CSI 和到期 reaper 服务。root 所有的环境文件提供 `ORGANIZATION`、`DATABASE_URL_FILE`、`CONFIG_FILE`。安装前按已验证节点调整路径。示例使用四个槽位，允许 180 秒优雅停止，超时后 systemd 可以强制结束进程。这个期限限制服务管理器的等待，不证明阻塞的物理文件系统 I/O 必然在该时间内完成。

命令逐行输出 JSON，报告就绪、领取、取消、返回结果、结果未确认、轮询失败和停止。`finished` 统计返回 worker 结果的任务，也包含 Unknown 或中断结果，不等于执行成功数。日志不包含原始 stdout/stderr 或凭据；它只是操作观察，数据库日志才是持久事实。日志接收端需要持续读取，同步输出阻塞会延迟轮询和信号处理。

## 原子领取与恢复边界

Store 按创建时间、执行 ID 顺序选择精确组织及完整存储身份对应的最早 `Queued` 请求。选择和原有一次性派发准入共享事务与组织事件流锁。多个 worker 及显式单次派发不能同时取得同一执行的 attempt。迁移 24 为 Queued 记录增加部分索引，不修改历史请求。

事务重新检查原始凭据、当前授权、提交的 connection/background lifetime、Candidate 和写入租约，再提交派发意图和 Outbox。撤权或过期请求会取消，不向外部派发。Outbox 失败会回滚领取。取消不会伪造写入者释放证明，租约协调仍遵循已有语义。轮询、锁等待、派发和重启均不会延长原始队列期限。

重启只选择仍为 Queued 的请求，排除 Dispatching、CancelRequested、Unknown 和终态。领取超时结果不明确时，不按 ID 重试派发：已经提交的请求会被排除，已回滚的请求可被后续轮询领取。持久记录不能重建活的进程与 I/O 封闭证明；显式只读观察恢复仍可使用，已派发任务的自动恢复不属于本 worker 的范围。

## 验证与限制

PostgreSQL 契约覆盖完整存储身份隔离、并发争抢、WAL 重启、撤权、过期、Outbox 回滚和迁移保留。真实夹具通过操作员命令并发运行两个 gVisor 执行，在两个启动授权都已生成后发送 SIGTERM，要求取得两个完成封闭证明、输出发布和写租约释放。重启必须领取零个已完成请求；非法选项和陈旧 boot 配置不得消耗队列。原有执行故障场景与这些检查一起运行。

[固定源码的组件记录](../evidence/queued-execution-worker-2026-10-10.json) · [验证日志](../evidence/queued-execution-worker-2026-10-10.log)

本增量交付已准备 Candidate 的节点本地自动派发。完整 Computer 生命周期调度、自动排空恢复、跨节点 fencing、可续期的长时间执行预算、浏览器、ComputerView 和产品验收仍待实现。Computer `ready=false`，公开 `execution` 仍不支持，T01–T43 保持 `not_run`。
