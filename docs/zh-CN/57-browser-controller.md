# 57. Browser 持久启动控制器

受信节点运维人员现在可通过 `browser-dispatch-once` 消费[已排队的 App](54-app-admission.md)。迁移 32 在唯一一次 Pod 创建尝试前持久化固定计划，随后记录原 Pod UID、活节点 guard、一次启动授权、启动观察与真实进程停止。App 准入截止时间始终不变，历史记录不能重建创建许可或活 guard。

## 生命周期与授权

`Queued → Dispatching → Running → Draining` 与未派发的 `Queued/Blocked → Cancelled` 分开。Dispatching、Running、Draining 持续占用原 Computer/App、Sandbox 与 Profile 槽位。撤销原凭据、主体、连接或准入授权会让已派发实例进入 Draining；恢复权限不会复活实例。查询会核对准入及已登记 watchdog 的到期状态。创建、attach、启动或完成不确定时也不能通过取消排队接口释放槽位。

控制器从不可变准入快照读取 CPU、内存、固定镜像及网络引用，运维配置绑定 namespace UID、Node UID/boot 与 viewport。核对 Kubernetes 身份、私有 attach、保护原进程域并持久化 guard 后才能给予启动许可。回调再次检查原凭据、连接、grant、catalog、Computer 代次、完整 actor/scope 和活 watchdog。授权提交后才发给驱动，丢失回执不能重发启动。

首版固定运行窗口最长为 **arm 后 30 秒**，同时受原 App 截止时间限制。更长的准入预算不代表续租。控制器每 200 毫秒检查当前授权，数据库调用最长等待两秒；数据库受阻时停止授权，节点 watchdog 独立运行。该命令的连接池关闭同样限制为两秒，避免已取消但仍受阻的查询无限拖住运维进程退出。SIGTERM/SIGINT 请求收尾；控制器崩溃后保留日志与占用。本控制器不授予 observe/control 回调，也不派发 GUI 动作。

## 运维命令

```bash
agent-computer-server browser-dispatch-once --database-url-file /private/database-url --organization acme --instance-id INSTANCE --expected-revision 1 --config-file /private/browser-controller.json
agent-computer-server browser-inspect --database-url-file /private/database-url --organization acme --instance-id INSTANCE
agent-computer-server browser-drain --database-url-file /private/database-url --organization acme --instance-id INSTANCE
```

配置包含 `api_url`、私有 `ca_file`/`token_file`、现有 Kubernetes `deployment` 绑定，以及包含 `approved_image`、`viewport` 和[受信节点配置](56-browser-runtime.md)的 `browser`。在已绑定、具备合格运行时与 reaper 的 root 节点上执行。数据库和 Kubernetes 凭据不进入 Browser Pod。inspect 只读历史证据；drain 只请求单向收尾，不能单独确认物理停止。目前没有自动队列接管或创建重试。

控制器在各类结果后都尝试用原 guard 终止进程。只有保留原内核句柄的 `StoppedBrowser` 才能记录数据库停止项。Pod 删除另用普通 UID/resourceVersion 前置条件，API 不存在不能证明进程停止。数据库停止确认丢失时继续保留占用；私有节点 spool 可供诊断，但序列化回执不能制造活句柄。

## 剩余范围与验证

Running 记录最后一次接受的启动观察。控制器丢失后，查询按原 watchdog 期限收敛到 Draining，这一步不确认物理停止；`ready` 仍为 false，公开 Browser 能力仍未开放。Profile 当前是私有易失 tmpfs，进程停止不能证明检查点已持久化、释放槽位或允许 Computer 停止/恢复。连续调度、watchdog 续租、GUI 控制代次与动作日志、观察发布、profile 检查点/恢复和 ComputerView 仍待实现。

PostgreSQL 契约覆盖并发一次性派发、重启/丢失确认、原 UID、不可变历史、各类撤权、到期、拒绝取消和资源保留。显式 `manual_browser_controller_live` 验证实际 server、Store、K3s、Chromium 与节点 watchdog，包含正常到期、撤权、数据库受阻和控制器死亡。该测试的 PreparedComputer/存储前置证据明确为合成数据，不验收 Workspace/CSI 或 profile 检查点语义。只在显式一次性 VM fixture 中使用 `--ignored --exact` 过滤运行，控制器死亡场景验证实际传输关闭后原进程已不存在，不把 EOF 处理与 watchdog 到期隔离；独立 watchdog 故障隔离见[节点组件验证](56-browser-runtime.md)。捕获的运维输出仅在生产进程结束后读取。

[源码绑定验证记录](../evidence/browser-controller-2026-10-11.json) · [Test log](../evidence/browser-controller-2026-10-11-tests.log)
