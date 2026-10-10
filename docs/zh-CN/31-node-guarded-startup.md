# 31. 节点布防后的执行启动

## 31.1 已交付路径

[执行工作器](29-execution-worker.md)现已要求独立[节点 watchdog](30-node-watchdog.md)完成布防，之后才能签发 Candidate 启动授权。新增 Rust `agent-computer-node` crate 将经过认证的 Kubernetes 观测与本地 K3s/containerd/runsc 进程树、已准备 Candidate inode 关联。这是限定组合的单节点适配器，不接受租户提供的宿主路径或运行时命令。

工作器先获取原 Pod 的启动挑战并复核存储。Kubernetes 回读要求准确的 namespace、Pod UID/spec、配置中的 Node 名称/UID/boot ID、Ready 状态，以及唯一 Running、重启次数为零的容器。API 观测是不透明的类型值，尚不能证明宿主进程。节点适配器继续检查当前宿主 boot、CRI 容器和 sandbox 身份、runsc 运行时、固定监督器 argv、只读根、UID/GID 1000、no-new-privileges 及空 capabilities。

适配器从 Pod UID 推导标准 systemd Pod 父组，核对两个 OCI cgroup 路径、Sentry PID/启动 ticks，以及 Sentry 和两个 Gofer 均位于原 sandbox scope。实际 kubelet bind 必须为 `/var/lib/kubelet/pods/<UID>/volume-subpaths/<PV handle>/sandbox/2`，其 FUSE 数据目录须匹配准备回执中的 inode、所有者和权限。路径中间项是记录的 PV/CSI handle，而非挂载的逻辑名称。未知布局直接拒绝；verbose CRI 字段及此 runsc 布局不被视为所有运行时的通用保证。参见上游 [CRI 协议](https://raw.githubusercontent.com/kubernetes/cri-api/v0.37.1/pkg/apis/runtime/v1/api.proto)与 [containerd 状态实现](https://raw.githubusercontent.com/containerd/containerd/main/internal/cri/server/container_status.go)。

## 31.2 固定定时器与持久授权

绝对节点截止时间在节点检查之前，根据原派发 attempt 的剩余预算确定。摘要计算、运行时检查和布防均消耗该预算；数据库读取或 writer 续租不能重置它。宿主命令通过打开的 root 所有 ELF 句柄和配置的 SHA-256 固定二进制，清空环境，将输出限制为 1 MiB，并共享最多十秒的检查预算。watchdog 在独立 session 中运行，先设置内核定时器，再确认原 cgroup/inode/boot/截止时间。丢弃活句柄不会杀死或续期 watchdog；控制器仍存活时，由独立等待线程回收退出进程。

迁移 16 新增不可变 `execution_watchdog_arms`，固定 execution、已观测 Pod、node/boot、container、cgroup inode、证据摘要和过期时间。注册要求不可克隆的活 `ArmedGuard`、原派发 attempt 和当前授权；核对 Candidate inode/PV handle 与已注册 Pod 计划，写入仅含元数据的 Outbox 证据，并在提交前再次检查权限和剩余时间。同一 node/boot/cgroup inode 不能分配两次。

布防后工作器再次回读 Pod/Node 身份。授权签发要求同一活句柄和持久记录，剩余预算受原 attempt、数据库过期时间和节点定时器共同限制。SQL 同时阻止有计划的 Pod 在缺少匹配、未过期布防记录时获得启动授权。重启后读取序列化证据不能重建 `ArmedGuard`、重放授权或许可另一个进程。历史授权仍可读取，不伪造布防记录。每次执行仍进入 Unknown、writer 保持 Draining；本地报告或空 cgroup 均不能释放 writer。

## 31.3 运营配置

`execution-dispatch-once` 必须以 root 在指定节点的宿主命名空间、目标 Pod 子树之外运行。在[第 29 节](29-execution-worker.md)的私有 `execution` 配置中增加：

```json
{
  "node": {
    "node": {"name": "ac-component-node", "uid": "ACTUAL_NODE_UID", "boot_id": "ACTUAL_BOOT_UUID"},
    "k3s": {"path": "/usr/local/bin/k3s", "sha256": "sha256:VERIFIED_K3S_DIGEST"},
    "watchdog": {"path": "/usr/local/bin/agent-computer-watchdog", "sha256": "sha256:VERIFIED_WATCHDOG_DIGEST"},
    "runtime_socket": "/run/k3s/containerd/containerd.sock",
    "spool": "/root/agent-computer/watchdogs"
  }
}
```

所有占位值必须替换为部署现场观测值。可执行文件及各级父目录须 root 所有、组及其他用户不可写；spool 须私有且 root 所有，适配器会在其中创建并移除私有请求文件。本地运行时 socket 属于特权运营访问，绝不挂入沙箱。Kubernetes 权限仅增加对指定 Node 的 `get`；[fixture RBAC](../../deploy/testing/node-watchdog-rbac.yaml)限制为 `ac-component-node`。其他环境须使用自己的 namespace 和身份。

构建命令为 `bazel build //crates/node //crates/watchdog:agent-computer-watchdog //crates/server:agent-computer-server`。恢复只观测并条件删除原 Pod，不根据存储 JSON 重新布防。

## 31.4 验证与未完成项

十项新增默认测试覆盖 Kubernetes node/boot/container 身份、结构化 CRI 拒绝、固定句柄执行、有界子进程 I/O、错误清理期间的子进程身份、SQL 身份/期限约束、不可变 WAL 恢复和迁移 16。SQL fixture 明确使用合成元数据，不构造节点活句柄。最终源码通过全部 303 项默认 Cargo/Bazel 测试，其中 PostgreSQL 134 项，分布于十个 Bazel 测试目标。

显式 `//crates/worker:execution_worker_live_test` 现使用一个 70 GiB 保留卷中的七个 10 GiB Candidate，覆盖库/命令执行、取消、丢失 Pod 确认、镜像拒绝、控制器 SIGKILL，以及控制器 SIGKILL 加沙箱 PID 1 STOP。最后一个场景先证明两项故障后写入者仍继续运行，再在任何 API 清理之前检查原截止时间后的固定 cgroup 递归空状态。普通控制器退出场景也可能由独立 PID 1 预算完成终止；只有暂停 PID 1 的场景隔离验证了外部 watchdog。root 组件实测及精确源码证据与默认测试分开记录。

这份原始运行证据没有认证 watchdog 自身崩溃监督或后续节点到期服务/重启协议。多节点路由、远程节点认证、终止后的进程准入封闭、异步存储排空、输出对象接纳、公开执行授权和完整 Computer Ready 仍待实现。宿主运营方和运行时仍是信任边界。`EmptyObserved` 是某一时刻的观测，不是持久 fencing。自动恢复与生产部署认证仍待实现；T01–T43 保持 `not_run`。

提交 `fd672ef` 中子进程回收修复后的最终源码通过全部七个真实场景，相同未变更的 watchdog 二进制通过十二个内核探针。另一个全新只读 JuiceFS 客户端通过两次 S3 GET 回读两个保存文件，数据库取证保留五份布防/授权及零完成/排空记录。[组件证据记录](../evidence/node-guarded-startup-2026-10-10.json)与[原始日志](../evidence/node-guarded-startup-2026-10-10.log)保留精确源码/二进制摘要、完整 VM 回读及早期失败。取证后已停止本次 QEMU 进程并移除其十个私有 VM 文件。

复核发现宿主 PID 复用窗口：`try_wait` 可能在错误清理向旧进程组 ID 发信号之前回收 CLI 主进程。适配器现使用 `waitid` 的 `NOWAIT` 观测退出，在子进程身份仍被保留时发送清理信号，之后才回收。此修订的真实子进程回归、完整默认测试、工作区 Clippy 及 VM 复验均已通过；记录中仍分别保留早期运行时观测的源码范围。

当前适配器要求两个进程独立布防并共用原始截止时间，见 [32 冗余节点 watchdog](32-redundant-node-watchdogs.md)。上文单 watchdog 运行证据仍绑定其原始源码版本。

常驻到期回收服务与独立回收报告见[34](34-node-expiry-reaper.md)。
