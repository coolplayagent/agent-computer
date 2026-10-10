# 33. 持久化 watchdog 观察

## 33.1 授权前保存意图，终止后保存报告

[双节点 watchdog](32-redundant-node-watchdogs.md)现在分别在配置的节点 spool 下保留私有日志。watchdog 布防前发布 `intent.json`，记录固定请求和诊断 PID，同步文件及目录，并在布防回执中返回日志引用。引用包含目录 device/inode 及意图原始字节的 SHA-256。节点适配器回读记录，核对请求和子进程 PID 后才返回活句柄。现有数据库布防摘要覆盖两份引用，无需数据库迁移；历史记录仍可读取，不伪造本地日志。

终止始终使用原始绝对期限。日志准备消耗该预算，过期后不能获得启动授权。定时器布防后，等待和终止路径不执行日志 IO。watchdog 先终止固定 cgroup 并进行有界内核观察，再发布 `report.json`，最后尝试写入回执管道。控制器关闭管道可能丢失最后一帧，但不会删除已经同步成功的报告。报告写入失败不返回成功 CLI 确认，也不授权替代执行。

发布使用排他临时文件、文件同步、`renameat2(RENAME_NOREPLACE)` 和目录同步，不覆盖已有正式记录或中断留下的临时文件。读取方也同步已打开的文件/目录，处理 rename 已可见但写入方尚未完成目录同步的窗口。操作依据 Linux [fsync](https://man7.org/linux/man-pages/man2/fsync.2.html) 和 [rename](https://man7.org/linux/man-pages/man2/rename.2.html) 契约，底层存储必须兑现这些保证；这不是经过断电实验的 SLA。

## 33.2 恢复输出

现有运维命令现在还会读取原始布防记录引用的日志：

```bash
agent-computer-server execution-recover-once \
  --database-url-file /private/database-url \
  --organization ORG --execution-id EXECUTION \
  --config-file /private/execution-config.json
```

在配置的原节点上以 root 运行，使用原 spool。命令先降低数据库权限并有条件清理原 Pod，再通过 `watchdog_journals.guards` 返回两份观察：

| 状态 | 含义 |
| --- | --- |
| `recorded` | 已读取有界且身份一致的本地报告；报告观察值为 `EmptyObserved` 或 `Unknown` |
| `unconfirmed` | 原意图有效，但不存在最终报告；`.pending` 文件不算完成 |
| `unavailable` | 本地记录、权限、摘要或身份绑定无法验证 |
| `legacy_unjournaled` | 历史布防记录没有日志引用 |

没有布防记录时，`watchdog_journals` 为 null。读取器整体失败通过 `node_error` 返回。异步读取在清理后有五秒等待预算，但不能取消已经阻塞的文件系统调用，运行时退出仍可能等待该线程。可重复查询恢复观察，但不会启动 watchdog、向 PID 发信号、重建许可、接受执行输出或释放 writer。派发结果不等待最终日志，这个字段保持 null。

读取 API 返回观察快照，不返回可写日志句柄；写句柄被单次运行消费。报告必须匹配固定请求/引用、原布防时间、cgroup device、时间顺序及一致的观察/错误字段。PID 仅用于诊断，不提供发信号的权威。

## 33.3 节点存储要求

沿用 [31](31-node-guarded-startup.md) 的私有 `node.spool` 配置，放在节点本地持久存储。新日志目录显式使用 `0700`，文件使用 `0600`。每级路径必须由 root 拥有且不可被组或其他用户写入；拒绝穿越和符号链接。读取仅接受普通单链接文件，不允许跨越子挂载，每条记录最多 8 KiB；每次读取都复核目录身份和意图摘要。

独立可信调用也支持 `agent-computer-watchdog --request PATH --journal EXISTING_PRIVATE_DIRECTORY`。省略 `--journal` 保留旧组件探针协议；生产节点启动始终要求日志及匹配引用。安装新版时同步更新配置中的 watchdog 二进制摘要。

日志在控制器/子进程退出和部分准备失败后保留。不要删除活跃或未决日志。自动保留策略、配额、跨节点复制及持久节点监督仍待实现。旧 boot 的报告仅是历史证据；设备/路径变化可能使其不可用，恢复动作不会使用旧 PID。

## 33.4 验证边界

默认测试覆盖原子非覆盖发布、中断记录、大小限额、受限 ID，以及报告身份/时间/错误一致性。root 专用双进程 fixture 还验证：杀死或暂停任一 watchdog 并关闭读取端后，存活者仍保存报告；篡改意图/报告身份、错误 PID、过宽权限、硬链接、符号链接和未完成报告不会被接受。

在一次性 root VM 中运行 [32](32-redundant-node-watchdogs.md) 的双进程 fixture，并执行：

```bash
python3 crates/watchdog/tests/journal_component.py \
  --watchdog /absolute/path/agent-computer-watchdog --output /private/journal-result.json
python3 crates/watchdog/tests/component.py \
  --watchdog /absolute/path/agent-computer-watchdog --output /private/kernel-result.json
```

日志探针覆盖正常持久化、关闭/写满的回执管道、报告发布失败、重复意图及非私有目录拒绝。这些组件测试不等于重跑完整 Kubernetes/CSI/Candidate worker 链路，不证明存储排空或持久 fencing。执行继续为 Unknown，writer 继续为 Draining，T01–T43 保持 `not_run`。

2026-10-10 的[源码绑定记录](../evidence/watchdog-journals-2026-10-10.json)和[原始日志](../evidence/watchdog-journals-2026-10-10.log)记录了 310 项默认测试通过（含 136 项 PostgreSQL 测试），以及 22 个 root VM 组件场景：4 个双守卫故障、6 个日志 IO 场景和 12 个内核回归。初次目录权限失败及显式 `0700` 修复均已保留。最终 VM 二进制与本地 Bazel 哈希一致，测试 VM、私钥和可写磁盘已清理。
