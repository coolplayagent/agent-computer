# 30. 节点 cgroup watchdog

## 30.1 已交付组件

`agent-computer-watchdog` 是用 Rust 与 Bazel 构建的一次性 Linux 宿主进程。它在可信运营方指定的专用工作负载 cgroup 之外运行，按固定截止时间终止该子树，独立于执行控制器和容器内 PID 1。组件不启动工作负载，也不访问 PostgreSQL、Kubernetes 或运行时 socket。[31 本地节点适配器](31-node-guarded-startup.md)现已在执行工作器签发启动授权前调用它。

请求固定执行关联 ID、当前节点 boot ID、`/sys/fs/cgroup` 下的规范相对路径、预期 cgroup inode，以及绝对 `CLOCK_BOOTTIME` 截止时间。未来截止时间不得超过 30 秒；已经过期的请求立即终止目标。重试相同请求不会重新计算预算。执行 ID 只用于关联，不能证明 cgroup 属于该执行。

## 30.2 内核边界

组件要求 root 身份和真实 cgroup v2 挂载，拒绝层级根、自身 cgroup 或祖先、非规范路径、boot/inode 不匹配、线程化域，以及路径上非 root 所有或组/其他用户可写的迁移控制文件。`openat2` 禁止符号链接、路径穿越和跨越后代挂载。打开的目录与控制文件句柄固定原始 cgroup；删除并复用相同路径不会把 watchdog 转向新进程组。

组件先设置绝对 `CLOCK_BOOTTIME` timerfd，再发出 `armed` 回执。该时钟计入系统休眠时间，不受墙上时钟调整影响，但不会唤醒休眠机器，也不保证调度延迟。回执管道关闭或已满时立即终止目标。CLI 使用不超过 4096 字节的原子非阻塞管道帧，控制器的输出背压不会推迟终止。stdout 必须是管道，不能直接连接终端或普通文件。

到期后向固定的 `cgroup.kill` 写入 `1`，再轮询固定的 `cgroup.events`，最多观察五秒。终止请求成功且递归状态为 `populated 0` 时报告 `EmptyObserved`；终止失败、观测丢失或观察超时均报告 `Unknown`。验证并打开目标组之后发生初始化错误，也会尝试终止。[内核 cgroup v2 文档](https://www.kernel.org/doc/html/latest/admin-guide/cgroup-v2.html)定义整组终止和递归存活状态；[timerfd 手册](https://www.man7.org/linux/man-pages/man2/timerfd_create.2.html)定义时钟及绝对定时器语义。

## 30.3 可信调用与限制

用 `bazel build //crates/watchdog:agent-computer-watchdog` 构建。可信节点运营方将有界 JSON 文件传给 `agent-computer-watchdog --request PATH`，通过管道读取两行 JSON：`armed` 与最终观测报告。报告包括固定请求、设备标识、启动/终止/观测时间、触发原因和观测状态。退出码 0 表示已输出本地 `EmptyObserved` 报告；退出码 2 表示请求拒绝、观测未知或输出不可用。缺失输出不能证明终止没有发生。

运营方必须在宿主初始命名空间中运行组件，将其置于目标子树和控制器生命周期之外，独占 cgroup 迁移/准入权限，并使用启动前固定的截止时间。CLI 会创建独立 session，尚不提供服务监督、经过认证的远程协议或崩溃恢复；执行布防持久注册由节点适配器/数据库集成完成。watchdog 自身被杀死/暂停、内核故障和节点分区仍需可信恢复。重放请求不是生产恢复协议。

`EmptyObserved` 只是某一时刻的本地内核观测，不能阻止后续进程进入，不能证明异步存储操作已排空，也不建立 Pod/container/Candidate 映射、接纳命令输出或释放数据库写入者。[31](31-node-guarded-startup.md)已交付本地 Pod/运行时/cgroup 身份和启动授权前的持久布防；生产运行仍需持久节点服务注册、监督与崩溃恢复、存储 fencing 和数据库对账。公开运行时仍不支持执行；[29 的流程](29-execution-worker.md)继续保留 Unknown/Draining。T01–T43 仍为 `not_run`。

## 30.4 验证

五项默认契约测试覆盖绝对截止时间、boot 身份、自身/祖先保护、严格有界请求解析和明确的递归空组判定。本增量交付时默认工作区有 293 项测试，分布于九个 Bazel 测试目标；当前数量见 [31](31-node-guarded-startup.md)。

以下显式 root 测试必须在可销毁 VM 中运行：

```bash
python3 crates/watchdog/tests/component.py --watchdog /absolute/path/agent-computer-watchdog --output /private/path/kernel.json
python3 crates/sandbox/tests/rootfs.py --supervisor /absolute/path/agent-computer-sandbox --destination /private/path/rootfs
python3 crates/watchdog/tests/runsc_component.py --watchdog /absolute/path/agent-computer-watchdog --runsc /absolute/path/runsc --rootfs /private/path/rootfs --work-dir /private/path/fresh-run
```

内核 fixture 覆盖监督进程停止与嵌套逃逸会话后代、冻结子树、过期截止时间、管道关闭/已满、控制器退出、cgroup 删除/复用、inode/boot 不匹配、自身祖先定位、已委派控制文件和线程化拓扑。gVisor fixture 先证明子进程在停止 PID 1、超过本地租约后仍继续工作，再验证外部 watchdog 终止真实 runsc/gofer 子树。这些组件探针使用本地 fixture 目录，没有真实 Candidate 或 Kubernetes 身份绑定。固定源码证据与默认测试分开记录。

显式探针在随后提交为 `2bde240` 的精确代码上通过 12 个内核场景和 1 个 gVisor PID 1 STOP 场景。[固定源码记录](../evidence/node-watchdog-2026-10-10.json)核对了 206 个源码/构建输入及精确二进制；[原始输出](../evidence/node-watchdog-2026-10-10.log)也保留先前线程化拓扑 fixture 的失败。gVisor 子进程确实越过 200 ms 本地租约继续运行，随后节点 watchdog 在固定截止时间终止运行时子树，内核在 10 ms 后报告无存活进程。自有 VM、磁盘和私有凭据已删除。这只构成组件证据，没有完成接纳、写锁释放或产品验收。
