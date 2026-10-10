# 29. 数据库授权的 Candidate 执行工作器

## 29.1 已连接链路

可信 `execution::execute_once` 工作器连接[派发准入](24-execution-dispatch.md)、[Pod 身份日志](28-execution-pod-journal.md)、[Candidate 挂载](27-candidate-pod-mounts.md)与[启动授权](25-execution-startup.md)。它消费一条已排队执行；重复调用在派发日志处失败，不中断正在工作的控制器。当前没有通用轮询调度器或后台执行存续期。

`candidate_execution_runtime_inputs` 重建原准备请求、输入清单、收据、Sandbox 快照和成功的 Volume 供应效果，检查固定摘要、输入版本、generation/Candidate 身份及已记录的 PVC/PV UID。它不解析当前资源头版本，也不采用原启动人的凭据。撤销凭据或禁用目录后，可信恢复仍能读取这些输入；读取本身不授予权限。

工作器使用与供应流程相同的规范化 Volume 计划，编译固定 Candidate 启动 Pod。运营方批准的镜像、StorageClass 映射、实际命名空间 UID、记录的 PVC/PV 名称以及 UID/GID 1000 必须匹配。登记计划前打开真实本地 JuiceFS 挂载并复核准备收据、inode、文件系统 UUID 和目录配额。收到监督器挑战后、申请新鲜数据库授权前，再次检查存储。

Pod 计划在唯一一次 POST 前提交。返回 UID 先持久记录，再等待 Running。等待和收集有界报告期间，控制器轮询当前执行权限。attach 只使用一次，不重连、不重发授权、不回退到 exec。原派发预算约束整个流程，包含编译、存储检查、调度、attach 与授权。监督器另从发出挑战之前扣减授权预算。

## 29.2 结果与恢复

每次尝试最终都会将执行权限降为 Unknown，并保留 Draining 写入租约。关联校验通过的本地报告仅作为未来收集器使用的私有原始字节返回，不构成完成接纳，也不进入运营命令 JSON。未得到报告时，`interrupted_at` 标记中断阶段，包括创建 Pod 之前的输入或存储拒绝。

清理观察原计划与已知 UID，采用 UID/resourceVersion 条件删除。创建响应丢失时，只能接纳与持久计划完全匹配的观察。对象不存在为 `api_absent`，删除请求已提交为 `delete_requested`，观察或删除不确定则为 `unconfirmed`，均不释放写入权。数据库降权最多等待 5 秒，清理最多 15 秒，清理中的可选 UID 补记最多 1 秒，避免数据库故障无限阻止条件删除尝试。超时不证明进程停止。正在运行的配额复核位于阻塞线程池，子进程受配置期限约束；取消它的等待同样不能证明排空。

`recover_once` 是显式停止/对账操作：它可通过降权中断现有控制器，重编译并比对原清单，再观察和删除。它不创建 Pod、不 attach、不重放存储的授权，也不访问本地数据挂载。运营配置变化或身份不匹配会阻止清理，不会选择替代实例。

## 29.3 运营命令

命令需要私有数据库/Kubernetes 配置及可信运营权限：

```sh
agent-computer-server execution-dispatch-once --database-url-file /private/control-url --organization ORG --execution-id EXECUTION --expected-revision 1 --config-file /private/execution.json
agent-computer-server execution-recover-once --database-url-file /private/control-url --organization ORG --execution-id EXECUTION --config-file /private/execution.json
```

私有 JSON 包含 `api_url`、`ca_file`、`token_file`、`deployment` 和 `execution`。传输与部署字段沿用 [12 Kubernetes](12-kubernetes-adapter.md)。`execution` 包含 `approved_supervisor_image`、[13 卷供应](13-volume-provisioning.md)的已核验 `storage` 映射，[17 准备工作器](17-candidate-preparation-worker.md)的 `candidate` 配置，以及 [31 节点布防](31-node-guarded-startup.md)要求的本地 `node` 绑定。执行不使用其中的对象缓存，也不重新物化输入。秘密通过文件引用提供，不进入 Pod 或命令输出。

## 29.4 验证与限制

新增 3 项 PostgreSQL 契约覆盖固定输入重建、WAL 恢复、撤权后的只读恢复和缺失/不匹配卷证据拒绝。默认 Cargo/Bazel 测试共 288 项，其中 PostgreSQL 131 项；Clippy、格式、双语文档和现有完整 Qualitygate 策略另行验证。

手动目标 `//crates/worker:execution_worker_live_test` 使用真实控制 PostgreSQL、Kubernetes/CSI、JuiceFS 和 gVisor。最初五个场景覆盖库调用、运营命令、子进程写入标记后的取消、创建回执丢失后的仅观察清理，以及监督器镜像拒绝；检查持久启动授权、持续写入互斥、禁止替代派发和零虚构排空。仅在一次性 root 所有的环境设置 `AGENT_COMPUTER_EXECUTION_TEST_CONFIG` 运行；配置在 [17 的夹具](17-candidate-preparation-worker.md)上增加 `image`、`server_binary` 和 `result_file`。测试目标存在不等于实测通过，实际运行必须另有固定源码证据。

公开运行能力仍报告执行不支持。生产监督器打包、watchdog 监督、持久物理 fencing、持久有界输出对象、完成接纳、自动崩溃对账和后台存续期仍待实现。本地轮询不能解决控制器或 PID 1 暂停。T01–T43 保持 `not_run`。

原始实测夹具为五个固定 10 GiB Candidate 配置 50 GiB Volume。最初的 4 GiB 配置在创建执行 Pod 前被容量准入正确拒绝。可选 `node_observation_file` 为独立[节点检查器](../../crates/worker/tests/execution_node.py)启用最多 8 秒的取消屏障：在一次性 VM 内以 `--observation PATH --output PATH` 并行运行。检查器验证实际 runsc 容器和 kubelet Candidate 挂载 inode 后才释放屏障，执行预算持续消耗。

五个场景已在 `bacaef9` 上实测通过。数据库回读保留 5 条派发、4 份 Pod 计划/UID 和 3 份启动授权，均关联原身份；5 条执行保持 Unknown，写入租约保持 Draining，完成接纳与排空记录均为零。节点检查确认取消场景的 runsc 容器及 Candidate inode 25。新的只读 JuiceFS 客户端通过两次 S3 GET 回读两个持久文件，共 18 字节。[固定源码记录](../evidence/candidate-execution-worker-2026-10-10.json)与[原始输出](../evidence/candidate-execution-worker-2026-10-10.log)绑定 223 个源码/构建输入、精确二进制/镜像，并保留此前的容量拒绝。取证后已回收自建 VM 及其私有凭据和存储。

[30 节点 watchdog](30-node-watchdog.md)已提供独立 cgroup 终止组件。[31 节点布防后的启动](31-node-guarded-startup.md)现已连接精确本地节点/运行时/Candidate 身份和授权前持久布防。当前夹具增加控制器 SIGKILL 与 PID 1 STOP 场景，在 70 GiB 卷上准备七个 Candidate，私有配置也须包含 `node`。
