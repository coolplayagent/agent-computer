# 25. 一次性执行启动授权

## 25.1 已交付行为

[24 派发日志](24-execution-dispatch.md)现在增加独立启动边界。迁移 14 为同一执行、Pod UID 和监督器挑战持久保存一份授权。隔离监督器支持 `--startup BOOTSTRAP_PATH`：启动任何应用进程前先发新挑战，等待一个有界 JSON 响应，再按剩余预算运行固定命令。原 `--request` 仍是组件入口，不是经过鉴权的产品 API。

可信控制器方法 `authorize_candidate_execution_startup` 在收到挑战后重新核验连接原主体/凭据、当前 scope 和资源权限、Candidate/目录绑定及固定期限。因此，调度延迟的 Pod 或旧派发尝试不能重新获得一段执行窗口。服务仍报告执行不支持：Kubernetes attach、可信监督器交付和实际 Candidate 挂载尚未连接。

## 25.2 协议与时间计算

不可变 Bootstrap 包含协议版本 1、已提交意图摘要和固定监督器 Request。其中 `lease_budget_ms` 只是初始上限，不能直接启动进程。PID 1 检查 namespace、非 root 与无 capability 条件，再生成 256 位随机 nonce。stdout 挑战绑定 nonce、执行/generation 和 Bootstrap 摘要。仅接受一个换行结尾、最多 64 KiB 的授权帧；未知或重复字段、错误版本、挑战摘要不匹配、零或超额预算、超大输入及额外完整帧均被拒绝。子进程 stdin 为空，不继承启动字节或控制凭据。

计时在 PID 1 发出挑战**之前**开始。可信控制器只有在收到该挑战后才读取新鲜数据库时间并计算剩余时长；PID 1 从先前的计时锚点扣除这段时长。因此，等待 attach、授权或响应传递都会消耗预算，收到授权不会重置计时器，也不需要比较 Pod 墙钟与数据库墙钟。响应到达时预算已耗尽，则在启动命令前拒绝。无授权等待最多 30 秒；等待期间收到 TERM、INT 或 HUP 也不会启动命令。

接纳后，命令超时从执行准备时开始计时；租约期限仍锚定在挑战之前，先到的期限生效。`StartupReport` 绑定挑战/授权摘要，包含原有本地监督报告，其 elapsed 时间包含启动等待。报告直接流式写入 stdout，不先把有界字节数组膨胀为 JSON 中间对象。应用 stdout/stderr 继续有界保留并独立排空。

挑战是公开关联数据，不是凭据或签名授权令牌。协议要求对精确、已核验 Pod UID 的独占鉴权 attach 通道，以及由运营方控制的监督器和 Bootstrap。仅靠握手不能让不可信镜像或任意 stdin 写入者变得可信；实际控制器必须建立这些部署前提。

## 25.3 数据库生命周期与失败处理

启动 API 只面向可信存储客户端，不接受租户指定主体、凭据、命令或预算。它从不可变派发输入派生 Bootstrap 并比对挑战，在同一事务内记录 Pod UID、挑战、固定授权、摘要和数据库时间，以及事件/Outbox。凭据/主体共享锁使撤销操作按序执行，写入后再次检查权限和期限。执行保持 Dispatching；`execution.startup_authorized` 明确包含 `process_started_confirmed: false`。

只有成功提交者得到 `ExecutionStartupAttempt`，该值不能克隆或反序列化。第二次授权调用返回 `DispatchAlreadyStarted`，包括换 Pod UID/nonce 或确认丢失的情况。只读恢复返回原记录，不续期。替代运行实例需要物理对账，不能再领取启动授权；控制器也不能把恢复数据当作重复外部操作的许可。

取消若先于启动授权提交，则阻止授权。凭据失效或原固定期限到期，会将已派发执行置为 Unknown，并保留 Draining 写入租约。续租不能延长启动期限。Outbox 失败或事务中凭据到期会回滚授权。升级不给既有派发凭空补授权。启动记录不可变，不提供进程成功或排空证据。

## 25.4 验证与后续工作

新增 3 项监督器契约测试验证摘要/nonce/预算绑定、严格输入和宿主拒绝。新增 6 项真实 PostgreSQL 测试覆盖授权互斥、WAL 恢复、运行身份变化、Bootstrap/generation 不匹配、版本冲突、派发后撤权/取消、续租后挑战延迟、Outbox/最终到期回滚和迁移 14。默认工作区共 263 项测试：121 项 PostgreSQL、20 项服务、8 项监督器及 114 项其他测试。Cargo test、fmt/Clippy、Bazel build/test 与中英文文档检查通过；完整 Qualitygate 保持既有换行策略。

另外，12 项真实 gVisor/Systrap 启动场景覆盖正常命令、延迟分片授权、过期授权、错误挑战、超额预算、EOF、授权前取消、重复/超大帧、锚定租约到期、子进程空 stdin/环境隔离和一直不给授权。相同二进制也通过原有 13 项监督器运行场景。这些显式组件运行使用测试授权帧和普通隔离工作目录，没有把数据库准入连接到 Kubernetes/CSI 运行时，不满足 T01–T43 验收。

仅在安装了已核验 runsc 的一次性 Linux VM 中复现：

```sh
bazel build //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-startup-rootfs
sudo python3 crates/sandbox/tests/startup_component.py --runsc /usr/local/bin/runsc --rootfs /tmp/ac-startup-rootfs --work-dir /var/tmp/ac-startup-component
sudo python3 crates/sandbox/tests/component.py --runsc /usr/local/bin/runsc --rootfs /tmp/ac-startup-rootfs --work-dir /var/tmp/ac-supervisor-regression
```

每个工作目录必须是新的。rootfs 脚本只对明确可信的本地二进制运行 `ldd`，生成的是验证 fixture，不是产品镜像。组件 JSON 记录精确 OCI 输入、运行时/二进制/库摘要及本地观测。

仍须实现严格的 Kubernetes Candidate Pod/attach 适配器与 worker、可信监督器打包、CSI 挂载身份核验、外部 watchdog 与物理 fencing、有界输出对象和权威完成接纳。监督器暂停故障仍意味着本地报告、启动授权或 Pod API 状态都不能释放写入租约。后台执行仍需独立生命周期。

[源码固定组件记录](../evidence/sandbox-startup-2026-10-10.json)与[原始输出](../evidence/sandbox-startup-2026-10-10.log)固定提交 `fca66d3`、196 个已核验构建/源码输入，以及实际监督器二进制 `sha256:7fd896307105722606b10081ef01e1a891fd4de2cc2ae253106ed14e8e40eba0`。两组测试运行同一二进制；取证后已删除测试 VM 及其私有磁盘/SSH 材料。

后续 [26 启动通道增量](26-kubernetes-startup-attach.md)已为临时 Pod 增加 hello 握手后的 Kubernetes 传输；Candidate 派发与物理 fencing 仍待实现。
