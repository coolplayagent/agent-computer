# 26. 有界 Kubernetes 启动通道

## 26.1 已交付行为

Kubernetes crate 新增 `StartupSandboxPlan`、`Client::attach_startup` 及一次性 `StartupChannel::run`，通过真实 Kubernetes WebSocket attach 传输 [25 启动协议](25-execution-startup.md)。本增量使用临时工作目录和运维明确认可的监督器镜像，尚未把数据库派发 worker 接到挂载 Candidate 的 Pod；公开执行能力仍为 unsupported。

计划固定监督器入口、不可变 Bootstrap、完整实例身份、镜像摘要及安全配置。运维认可的镜像必须与已验证 Sandbox 镜像完全相同，并包含可信监督器及其工具；租户镜像中恰好存在同路径文件不构成信任。Bootstrap 会出现在 Pod 参数与注解中，不能包含凭据。必须启用 `stdin=true`、`stdinOnce=true`、`tty=false`、`restartPolicy=Never`、独立进程 namespace、UID/GID 1000、无 capabilities、禁止提权及只读根目录。工作区为 64 MiB 内存 `emptyDir`；Candidate/CSI 挂载仍待实现。

## 26.2 启动顺序与身份

1. 重读部署前提与已记录 Pod 的精确 UID/spec，要求 Running。
2. 使用原有显式信任 HTTPS 客户端升级到 HTTP/1.1 WebSocket，要求 `v5.channel.k8s.io`、正确接受摘要及无扩展。禁止重定向、重试、TTY、exec 与旧协议回退。
3. 向 stdin 发送绑定 Bootstrap 摘要的有界 hello，它不授予执行权限。PID 1 收到后才产生新挑战，避免在 attach 接入 stdout 前写出挑战。hello 和 grant 分别最多等待 30 秒，每条输入行最多 64 KiB。
4. 读取并验证挑战，再次核对相同 Pod UID/spec。之后才允许未来的可信 worker 获取数据库新鲜启动授权；打开通道本身不产生数据库权限。
5. `run` 校验授权，再核对一次 Pod，然后只发送一次授权，并用 v5 `[255, 0]` 消息仅关闭 stdin。通道消费后不能克隆或反序列化，不重连、不重发。

Kubernetes attach 参数没有 Pod UID 前置条件，因此上述观测依赖运维独占的 namespace 与 attach RBAC，不能等价为原子 UID 前置条件，也不防御恶意集群管理员。见上游 [PodAttachOptions 定义](https://raw.githubusercontent.com/kubernetes/api/v0.37.1/core/v1/types.go)。v5 关闭流格式来自 [Kubernetes wsstream](https://pkg.go.dev/k8s.io/streaming/pkg/httpstream/wsstream)，通道编号来自 [remotecommand 常量](https://github.com/kubernetes/apimachinery/blob/master/pkg/util/remotecommand/constants.go)。

监督器仍从发出挑战之前的时间锚点扣减授权预算；客户端更保守地从 attach 开始之前计时，身份核验、数据库及传输延迟都会消耗可用时间。恢复读到数据库 grant 不意味着可以重新 attach 或重放。取消、撤权对账和独立外部 watchdog 仍属于待接入 worker。

## 26.3 上限与不确定性

| 输入或操作 | 上限 |
| --- | --- |
| attach、挑战与挑战后身份复核 | 合计 15 秒 |
| WebSocket 消息/帧 | 64 KiB；至多 4096 条入站消息，包含控制消息 |
| 挑战 | 4 KiB |
| 监督器诊断 | 64 KiB |
| remote-command 状态 | 4 KiB |
| 原始报告序列化字节 | `8 × 单流输出上限 + 16384` |
| 报告收集 | 原客户端预算期限 + TERM 宽限 + 2 秒 |

二进制流消息可跨帧拆分 JSON；文本消息、未支持通道、绑定错配、完整行之后额外非空白字节及超限数据均拒绝。报告只解析关联字段，不将输出字节数组展开成通用 JSON 树。

一旦尝试写出授权，写入、断连、超时、错误状态或报告收集不确定性均返回 `MutationUnconfirmed`，适配器不重试。写入前失败也不允许重发持久授权。丢弃 future 或连接不能证明进程终止、存储排空或租约释放；仍须独立 watchdog 与恢复所有者。

`StartupObservation` 包含 Pod UID、有界原始报告及诊断，只核对挑战、授权、execution、generation 和请求摘要。Kubernetes remote-command `Success` 表示传输完成，应用结果与持久完成接纳另行判断。有效本地成功报告仍不能 fence 已暂停监督器或排空 Candidate 写入者。

## 26.4 验证与复现

新增 8 项协议测试覆盖固定计划、挑战分帧、hello/grant 顺序、ping/pong、一次 stdin 关闭、三处 UID 替换、错误握手/挑战/授权、重定向、响应丢失、失败状态及输出上限。默认工作区共 271 项测试，其中 Kubernetes 26 项；Cargo test、fmt/Clippy 与 Bazel test 通过。完整 Qualitygate 仍只采用既有行尾策略。

手动目标 `//crates/kubernetes:startup_live_test` 在临时 K3s/gVisor VM 中执行四个场景：正常 stdout/stderr、命令超时、租约耗尽，以及 100,000 字节输出仅保留前 64 字节。节点检查确认 `io.containerd.runsc.v1`、Systrap 与只读 OCI 根目录。授权来自测试夹具，这些场景没有串联数据库授权、CSI 存储、外部 fencing 或产品完成。

在 Linux x86_64 上使用可信本地二进制构建测试镜像：

```sh
bazel build //crates/sandbox:agent-computer-sandbox //crates/kubernetes:startup_live_test --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-attach-rootfs
python3 crates/kubernetes/tests/startup_image.py --rootfs /tmp/ac-attach-rootfs --output /tmp/ac-attach-image.tar
```

将归档导入临时集群 containerd，使用脚本输出的摘要引用。应用[基础测试 RBAC](../../deploy/testing/kubernetes-component.yaml) 和 [attach 测试 RBAC](../../deploy/testing/startup-attach-rbac.yaml)，后者仅增加 `pods/attach`，不授予 `pods/exec`。提供私有 JSON 文件，包含 `endpoint`、`ca_file`、`token_file`、`namespace`、`namespace_uid`、`runtime_class_uid`、`deny_policy_uid`、`image` 和 `result_file`。可选 `observation_file` 会让第一个场景等待同目录 `.inspected` 标记，供节点检查使用，最长 15 秒。

```sh
bazel test //crates/kubernetes:startup_live_test --lockfile_mode=error --test_env=AGENT_COMPUTER_KUBE_STARTUP_CONFIG=/private/startup-config.json
```

夹具不能作为产品监督器镜像发布。可信镜像来源、Candidate 挂载、数据库派发 worker、独立 watchdog/fencing、输出对象和权威完成接纳仍待实现；运行验收 T01–T43 继续为 `not_run`。

[固定源码证据](../evidence/kubernetes-startup-attach-2026-10-10.json)与[原始输出](../evidence/kubernetes-startup-attach-2026-10-10.log)绑定提交 `d25fdd5`、200 项已核验输入及精确测试镜像/监督器二进制。同一二进制的 12 项启动回归和 13 项监督器回归也全部通过；VM、私有磁盘、SSH 密钥与 Kubernetes 凭据已清理。

后续 [27 Candidate 挂载增量](27-candidate-pod-mounts.md)增加已准备数据叶目录挂载；数据库 worker 接入和 fencing 仍待实现。
