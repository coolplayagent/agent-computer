# 41. 经 CSI 发布执行文件屏障

单次执行 worker 已把 [Candidate FUSE 边界](40-candidate-io-fence.md)接入真实 gVisor Pod。生产派发使用 `csi.agent-computer.io` inline 卷，启动授权前验证前置文件系统。工作负载命名空间继续采用 PodSecurity `restricted`；Pod 不接收 hostPath 或底层 JuiceFS 目录。

执行完成仍采用保守状态：`Unknown` 执行和 `Draining` 写租约尚未解除。本次集成没有把进程终止、持久输出与活体 I/O 屏障合成为受认可的完成事务。Computer `ready` 仍为 false，T01–T43 仍为 `not_run`。

## 发布与启动

数据库派发提交、本地准备回执与配额验证通过后，worker 在 `/var/lib/agent-computer-csi/mounts` 下为执行创建全新的私有挂载点，禁止复用。挂载引用记录随机实例、完整准备回执、设备号/inode、启动 ID 和挂载命名空间。反序列化引用不能重建活体 `MountedFence` 或 `SealedFence`。

Pod 日志把该引用及确切 Node 名称/UID/启动 ID 绑定到已准入执行。唯一一次 Pod 创建前，worker 在 root 私有本地登记表中持久登记活体挂载及预期 Pod 名称、命名空间和节点。CSI 发布器只接受预期的 Kubelet 临时卷上下文、服务账号、挂载能力与 workspace 目标路径。首次发布先持久记录该实例的唯一 Pod UID、CSI 卷 ID 和目标。即使 Pod 同名，更换任意一项也会被拒绝。

发布通过 `open_tree` 克隆已固定的 FUSE 树，再通过 `move_mount` 挂接到已固定的目标描述符。结果必须匹配记录中的设备、根、实例专属来源和实测的 `fuse` 类型。挂接前后均设为 private，因为挂接到 shared 宿主父挂载时可能产生新的共享组。确切重试只观察既有挂载，不叠加或替换。登记、首次领取与撤销标记使用有界 root 文件、同步写入和共享非阻塞锁；不完整或损坏记录拒绝发布，尚无自动垃圾回收。

启动命令授权前，节点适配器独立核验容器运行时的 workspace 来源确为 inline CSI 目标，并将设备号和根 inode 与活体 FUSE 句柄比较。watchdog 登记事务要求该观察与 Pod 日志及准入准备回执一致。原有 Pod/Node UID、运行时、cgroup、双 watchdog 和 reaper 检查继续生效。旧的直接 Candidate 计划编译器仅保留用于历史清理和组件夹具；派发命令没有直接挂载回退路径。

## 结束、丢失与恢复

派发尝试返回时，控制器立即关闭修改入口。随后降低数据库权限、登记本地发布撤销、请求条件删除 Pod、保存可用的有界输出并尝试封存。只有封存成功才返回 `io_fence` 审计元数据。超时、卸载、空 cgroup、进程报告或存储的引用均不能生成该结果，也不能释放写租约。

控制器死亡使其 FUSE 连接断开，既有描述符上的新写入也会失败。但死亡前已提交到底层文件系统的操作可能仍未解决，因此这不构成成功排空。独立 watchdog/reaper 仍负责存活进程。恢复仅按记录重建确切 Pod 计划用于清理、撤销后续发布并观察日志；不会重建挂载、重发授权或提供 I/O 封存证明。

CSI 卸载先持久撤销，再解除目标挂载。它无需失联的 FUSE 源响应文件系统调用，也能验证记录中的目标和挂载身份。重复清理已不存在的目标成功；外来或被替换的挂载保留并报告前置条件失败。发布器重启保留持久领取记录，无法重新打开已撤销实例。

## 操作者部署

构建 `//crates/csi:agent-computer-csi`，以 root 所有的受信节点程序安装。将经操作者验证的上游 [node-driver-registrar](https://github.com/kubernetes-csi/node-driver-registrar/tree/v2.9.0) 安装到 `/usr/local/bin/csi-node-driver-registrar`；组件记录固定了实测镜像摘要及提取程序的校验值。注册器负责 Kubelet 插件注册，Rust 服务负责 CSI Identity 和 Node RPC。Kubelet 注册客户端使用套接字路径作为 authority，严格的 Rust HTTP/2 栈会拒绝；其 CSI 客户端显式使用 `localhost`。采用上游注册器保留标准协议处理，参见[注册客户端](https://github.com/kubernetes/kubernetes/blob/master/pkg/kubelet/pluginmanager/operationexecutor/operation_generator.go)与 [CSI 客户端](https://github.com/kubernetes/kubernetes/blob/master/pkg/volume/csi/csi_client.go)。

安装[发布服务](../../deploy/agent-computer-csi.service)与[注册服务](../../deploy/agent-computer-csi-registrar.service)，创建 root 所有的 `/etc/agent-computer/csi.env` 并填写确切 `NODE_NAME`，应用 [CSIDriver 声明](../../deploy/candidate-csi.yaml)。将 unit 的 K3s 依赖与实际操作者服务名对应，再启用和启动 `agent-computer-csi.service`。派发前确认节点的 `CSINode` 已出现该驱动。服务创建私有登记表与插件套接字目录；重启间必须保留可靠本地存储上的状态。

发布器、合格的原生 JuiceFS 挂载及执行控制器必须位于同一宿主挂载命名空间。不要添加会隐式创建其他挂载命名空间的 systemd 文件系统隔离选项。CSI 套接字仅 root 可用，另核验对端 UID，且没有 TCP 监听。当前参数下上游注册器也不启动 HTTP 监听。未提供 Controller、Stage、Expand、块卷、自选文件系统/挂载选项或卷挂载组能力。第 40 节的文件系统操作限制继续适用。底层目录、特权节点进程及登记文件只对操作者开放。

## 验证与范围

[组件记录](../evidence/fenced-execution-csi-2026-10-10.json)和[日志](../evidence/fenced-execution-csi-2026-10-10.log)绑定实测源码与程序。默认测试覆盖上下文和能力拒绝、直接挂载替换、节点身份及清理挂载身份。显式 `execution_worker_live_test` 运行十一项真实 PostgreSQL/K3s/gVisor/JuiceFS/S3 场景，其私有配置需新增 `csi_test_binary`，指向构建的 `//crates/csi:csi_live_test`。取消场景调用确切活跃执行的检查，证明重试、替换 UID 拒绝及发布器重启保留同一挂载。控制器被杀场景验证旧描述符写入失败，同时停止的 PID 1 仍需独立计时器清除。

随后设置 `AGENT_COMPUTER_CSI_DISPOSABLE_TEST=1`，并把已结束 normal 执行的实例填入 `AGENT_COMPUTER_CSI_TEST_INSTANCE`，运行 `csi_live_test --exact real_csi_cleanup_replay_and_peer_boundary --nocapture`。它通过真实 Unix gRPC 验证重复清理、撤销后重放、错误身份、特意替换的外来 bind 挂载，以及 UID/GID 1000、无附加组客户端的拒绝。仅在明确的一次性宿主环境运行。独立新客户端 JuiceFS 与签名 S3 回读另行取证，不以控制器报告替代。

这属于单节点组件验证，不构成多节点 fencing、断电恢复、通用应用文件系统或完整 Computer 验收。登记表保留/回收和受认可执行完成仍需继续实现。
