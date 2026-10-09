# 22. Sandbox 进程监督器

## 22.1 已交付组件

`agent-computer-sandbox` 是已隔离执行容器中的一次性 Linux init。它执行结构化 argv、持续读取有界 stdout/stderr、回收被托管的后代进程，并在超时、取消或本地租约预算到期时请求终止。Cargo 与 Bazel 构建同一 Rust crate。目前尚未接入持久化 Execution 准入、Candidate 挂载、API、对象输出存储或写租约完成流程。

程序只允许在以下条件运行：自身为 PID 1，`/proc` 对应本 PID namespace，UID/GID 和有效 UID/GID 均为 1000，effective/permitted/bounding capabilities 为空，已启用 `NoNewPrivs`，且启动时没有其他可见进程。没有宿主执行绕过开关。部署方还须独立保证 gVisor、私有 namespace、只读根目录、受限挂载、网络/资源限额和无控制凭据镜像；二进制本身不能证明这些部署属性。

## 22.2 输入与执行

在容器中运行 `/bin/agent-computer-sandbox --request /request.json`。操作方提供本地普通请求文件，并将已准入目录挂到 `/workspace`。请求最多 64 KiB，拒绝未知或重复字段。例如：

```json
{
  "execution_id": "execution_1",
  "generation": 1,
  "argv": ["/bin/sh", "-c", "printf hello > result.txt"],
  "cwd": "",
  "timeout_seconds": 10,
  "lease_budget_ms": 15000,
  "term_grace_ms": 100,
  "output_limit_bytes": 65536
}
```

可执行路径必须为绝对路径；最多 128 个参数、合计 32 KiB。不隐式拼接 shell，脚本显式指定解释器。`cwd` 为空或规范化相对目录，最多 1024 字节/32 段；通过 `openat2` 在 `/workspace` 下取得禁止符号链接和跨挂载的目录描述符，子进程通过固定描述符切换目录。这限制初始 cwd，并不约束命令之后的所有文件访问；文件系统隔离仍依赖容器挂载边界。

子进程 stdin 为空，仅接收 `PATH=/usr/bin:/bin`、`HOME=/tmp`、`LANG=C`。环境引用和 stdin 引用尚未实现。监督器派生进程前设置 non-dumpable，阻止同 UID 子进程访问其内存和 `/proc/1/fd` 描述符；执行另有独立进程组。后续控制接线仍必须把任意解释器视为 modify 操作，不能根据命令文本判断只读。

`timeout_seconds` 范围为 1–3600，`lease_budget_ms` 为 1–30000，TERM 宽限为 0–5000 毫秒。单调时钟在 cwd 解析/派生前开始，取超时和租约预算中较早的截止时间。预算是可信调度输入，不是签名租约或授权；派发方必须保守扣除传输耗时，当前没有续租协议。阻塞挂载/exec 或监督器暂停都需要独立的运行时 watchdog。

## 22.3 终止与输出

监督器将 SIGTERM、SIGINT、SIGHUP 视为取消。进入停止后，反复向私有 PID namespace 中所有可发送信号的进程发信号，包括调用 `setsid` 或改变进程组的后代；宽限结束后使用 SIGKILL。每轮输出读取和 `wait(2)` 工作量均有上限，输出/fork 洪泛不能无限占用计时器。收到 `ECHILD` 并确认两个输出 EOF 后才结束，最后排空最多再等两秒；无法确认则为 `unknown`。取消 Rust future 也会请求 SIGKILL，但不据此宣称已完成回收。

主进程退出不足以完成执行：只要仍有后代存活，就清理并报告 `descendants_terminated`，即使主进程退出码为 0。正常成功要求退出码 0、全部后代已回收、两个输出 EOF 且未超过截止时间。忽略 TERM 的超时/取消场景可能同时有主进程 SIGKILL 状态和不同的本地结果；首次观察退出时已过期，则到期优先。设置失败退出 125；成功输出本地报告退出 0，因此程序退出码 0 本身不代表命令成功。

每个流保留 0–1 MiB 前缀，记录已观察字节数和截断标志，超额仍持续读取丢弃。有界报告包括请求摘要、execution identity/generation、主进程退出/信号、回收数量、耗时及二进制字节数组；这是本地采集格式，不向 PostgreSQL 写入输出字节。持久化分块、对象引用和带认证结果接受仍待实现。

## 22.4 信任边界与复现

**报告不是物理 fencing 或持久化写入排空证明，不能释放 Candidate 租约。** Linux 的 namespace init 语义与支持运行时的实际行为都需要验证；[Linux PID namespace 手册](https://man7.org/linux/man-pages/man7/pid_namespaces.7.html)说明 init 与进程终止规则。实测 gVisor `release-20261005.0` 中，同 UID 子进程的 `kill -STOP 1` 能暂停 init 及本地 watchdog。故障测试必须从外部运行时终止容器，并记录无本地报告、权威结果仍未知；不会把容器删除解释为产品 fencing。接入共享工作目录交接前，必须有可信外部 watchdog 和节点/fence 证据。

显式组件脚本为 [rootfs.py](../../crates/sandbox/tests/rootfs.py) 和 [component.py](../../crates/sandbox/tests/component.py)。fixture 复制本地可信二进制、指定 shell 工具和动态库，不是产品 OCI 镜像；`ldd` 步骤只用于可信本地二进制。构建命令：

```sh
cargo build -p agent-computer-sandbox --locked
bazel build //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py \
  --supervisor target/debug/agent-computer-sandbox \
  --destination /tmp/supervisor-rootfs
```

将 fixture 和组件脚本复制到装有固定 runsc 版本的一次性 Linux VM；仅在该 VM 内以 root 执行：

```sh
python3 component.py --runsc /usr/local/bin/runsc \
  --rootfs /root/supervisor-rootfs --work-dir /root/supervisor-component
```

脚本使用全新私有运行时状态，容器内非 root、无 capabilities，强制清理，并记录二进制/动态库摘要。13 项场景覆盖 argv 边界、环境清理、受限 cwd、符号链接拒绝、退出/派生失败、init 描述符保护、监督器暂停后的未知状态、超时、租约到期、取消、脱离进程组的后代及双流洪泛。测试使用普通临时目录和 `--ignore-cgroups=true`；实际 Candidate/CSI 绑定、cgroup 限额、Kubernetes 生命周期、数据库/网络分区、带认证执行结果及 T01–T43 运行验收仍未验证。
