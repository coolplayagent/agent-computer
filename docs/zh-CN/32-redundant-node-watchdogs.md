# 32. 冗余节点 watchdog

## 32.1 单进程故障保护

[节点适配器](31-node-guarded-startup.md)现在先布防两个 watchdog，之后才允许签发 Candidate 启动许可。两个进程通过同一已固定描述符的二进制启动，分别创建独立会话，确认相同 execution、boot、cgroup 路径/inode 和绝对 `CLOCK_BOOTTIME` 截止时间；cgroup device 观察也必须一致。启动第二个进程消耗原有检查与执行预算，不延长任何定时器。

活句柄在注册、许可签发和 worker 每次权限轮询时都要求两个子进程可用。`waitid` 使用 `WNOWAIT` 观察退出或暂停状态，在回收前保留原进程身份。任一进程退出或暂停都会阻止授权，即使另一个仍已布防。控制器消失后，只要另一个 watchdog 存活，就仍可在同伴收到 SIGKILL 或 SIGSTOP 后按原期限终止目标。第二个进程启动失败时，第一个定时器保持布防，调用不返回有效双进程句柄。

句柄释放会关闭回执读取端并分离回收线程，不向 watchdog 发送终止或续期信号。两个定时器不依赖同伴通信或控制器心跳。单个 watchdog 的请求/回执协议及固定期限保持不变。

## 32.2 持久化注册与迁移

迁移 17 要求新布防记录使用版本 2 节点证据，包含原回执、`backup_armed`、两个不同的正数 PID，以及早于两者共同期限的观察时间。PID 只用于诊断，不能作为重启后发信号的权威。活句柄仍需通过 [31](31-node-guarded-startup.md) 的 Pod/runtime/Candidate 身份绑定；SQL fixture 不能证明实际绑定。

完整双进程证据纳入现有不可变布防摘要。Store 注册布防和签发启动许可时，两个进程仍必须可用。数据库有效期和启动预算继续受原始 attempt 与节点截止时间限制。

迁移保留历史布防/许可及其摘要。升级后的新许可不能使用历史单进程布防。重启恢复不能重建活句柄、替换失效定时器、重置期限或释放 writer。存储排空、输出验收和 writer 释放仍需继续实现。

## 32.3 验证

默认测试覆盖真实子进程退出/暂停、句柄分离、PostgreSQL 缺失或不匹配的备用回执、重复或越界 PID、WAL 回读和迁移兼容性。显式内核 fixture 调用生产双进程启动器，使用四个新建且归本次测试所有的 cgroup：分别杀死或暂停任一 watchdog，关闭两个回执读取端，检查存活定时器是否在原期限终止暂停的 workload 并使 cgroup 变空。

只在具有可写 cgroup v2 的一次性 Linux VM 中以 root 运行：

```bash
bazel build //crates/node:node_contracts_test //crates/watchdog:agent-computer-watchdog
AGENT_COMPUTER_WATCHDOG_BIN=/absolute/path/agent-computer-watchdog \
  /absolute/path/node_contracts_test --ignored --exact \
  guard::tests::redundant_timers_survive_either_guard_killed_or_stopped --nocapture
```

先将二进制上传到该 VM，再执行验证命令。默认 Cargo/Bazel 测试会忽略这个 root 专用 fixture。内核观察属于组件证据，不等于 Computer 完整运行验收。

## 32.4 剩余边界

本能力容忍一个 watchdog 进程失效，不提供持久节点服务、自动重启、多节点路由、存储排空或持久 fencing。两个进程共享主机及故障域：主机故障、共同父 cgroup 被冻结、管理员同时终止两者，或相同二进制/内核故障仍可能使两者失效。独立会话不隔离服务 cgroup 的清理；部署必须将两个 watchdog 放在 workload 和控制器清理组之外。暂停的 watchdog 需要可信清理，分离的回收线程不会恢复或替换它。

`EmptyObserved` 仍只是某一时刻的观察，不能阻止后续进程准入，也不能证明异步存储已完成。执行仍为 Unknown，writer 仍为 Draining；公开执行能力和 T01–T43 验收状态不变。
