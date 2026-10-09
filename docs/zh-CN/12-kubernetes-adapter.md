# 12. 受限 Kubernetes 适配器

## 12.1 已交付范围

Rust `agent-computer-kubernetes` 库可通过真实 Kubernetes HTTPS API 创建、核对并有条件地删除经显式准入的**临时** Sandbox Pod。尚未连接协调队列：apply Sandbox 定义仍不会启动 Pod。Computer 运行准入、持久化实例分配、Workspace Candidate、JuiceFS 挂载、租约 watchdog 与物理 fencing 待实现，公开运行时能力仍为 unsupported。

`EphemeralSandboxPlan::new` 接收已通过静态验证的 ComputerSet、Sandbox 名称、已持久化的组织/Computer/Sandbox/实例身份、generation、SpecVersion revision、命名空间及命令参数数组。这些值不代表权限。后续 worker 必须先授权启动、持久化实例分配，并获得已有派发许可，才能调用适配器。租户 HTTP API 不暴露这些参数。

目前仅支持 `gvisor`，以及部署登记映射到命名空间全拒绝 NetworkPolicy 的策略；拒绝 Workspace 挂载。Pod 固定声明中的镜像摘要与 CPU/内存请求和上限，使用 UID/GID 1000、移除全部 capabilities、禁止提权和宿主命名空间、启用 `RuntimeDefault` seccomp 与只读根文件系统，关闭服务账号令牌自动挂载。两个各限 64 MiB 的内存卷提供 `/tmp` 和 `/dev/shm`。不自动重启，存活期限一小时，删除宽限期 30 秒。此实现尚不是浏览器镜像或持久化 Computer 环境。

## 12.2 身份与传输

Pod 名称由组织和已持久化实例 ID 确定。同一实例修改 generation、revision 或命令会产生冲突，不会创建另一个 Pod。标签与完整绑定 annotation 将读回对象关联到全部计划输入；已知 UID 时必须核对实际 Pod UID，同时核对 resourceVersion 与受控 spec 字段。仅接受有限的 Kubernetes 默认字段；拒绝注入容器、挂载、环境变量或降低安全限制。默认开启的安全相关开关必须显式关闭；可能影响运行时的额外 annotation 也会被拒绝。

create/observe 前核对命名空间实际 UID、Active/restricted 状态、`gvisor` RuntimeClass UID 与 `runsc` handler，以及唯一 NetworkPolicy 的 UID 和全拒绝规则。部署运维方须限制命名空间/策略变更并配置真正执行规则的 CNI。API 读取不证明节点执行或网络隔离，也不能消除与受信集群管理员之间的变更竞态。

客户端只接受显式 HTTPS origin、CA、bearer token 和部署对象 UID，不读取 kubeconfig、不运行凭据辅助程序。仅信任提供的 CA，关闭代理、重定向和自动重试，连接/请求总超时分别为 5/15 秒，响应限制 1 MiB；错误不包含上游响应正文、URL 或凭据。

创建仅发送一次 POST。冲突需读回；响应丢失或成功响应无效时返回 `MutationUnconfirmed`。恢复必须核对原持久化身份，不能另分实例或盲目重新派发。删除同时携带 UID 与 resourceVersion 前置条件，不进行零宽限强制删除。API 删除/缺失以及 Pod phase 都只是观察事实，不得据此释放 Workspace 写者或证明分区节点的进程已停止。见 [11 持久化协调](11-reconciliation-coordination.md) 与 [Kubernetes Pod 生命周期](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/)。

## 12.3 验证

默认测试包含 13 项适配器测试，覆盖创建响应丢失且不重复 POST、身份冲突、spec 注入、前置核对拒绝、条件删除、响应限额与错误脱敏。这些是协议测试，不是运行时验收。

显式组件测试需要安装 runsc 并启用有效 CNI 的隔离集群。[测试部署清单](../../deploy/testing/kubernetes-component.yaml) 创建专用命名空间、RuntimeClass、全拒绝策略和受限服务账号，仅用于可销毁测试集群。由运维方获取 `ac-adapter` 的限时 token、集群 CA 和对象实际 UID；凭据须以私有权限存放在仓库外。

创建私有 JSON 配置：

```json
{
  "endpoint": "https://127.0.0.1:16443/",
  "ca_file": "/private/component/ca.pem",
  "token_file": "/private/component/token",
  "namespace": "ac-kube-component",
  "namespace_uid": "actual-namespace-uid",
  "runtime_class_uid": "actual-runtimeclass-uid",
  "deny_policy_uid": "actual-networkpolicy-uid",
  "image": "docker.io/library/busybox@sha256:REPLACE_WITH_VERIFIED_DIGEST"
}
```

```bash
bazel test //crates/kubernetes:kubernetes_contracts_test
bazel test //crates/kubernetes:kubernetes_live_test \
  --test_env=AGENT_COMPUTER_KUBE_TEST_CONFIG=/private/component/config.json
AGENT_COMPUTER_KUBE_TEST_CONFIG=/private/component/config.json \
  cargo test -p agent-computer-kubernetes --features live-test --test live --locked
```

真实测试检查 TLS、实际创建/读回/冲突、Pod Running 观察和条件删除。可选 `observation_file` 会等待外部检查器至多 60 秒，由其收集日志与节点 runsc 证据，再建立同路径、扩展名改为 `.inspected` 的文件。另需检查容器日志 `AC_SECURITY_PROBE_OK` 和实际 containerd runtime。缺少显式配置时测试失败；Cargo 默认测试通过 feature 排除它，`bazel test //...` 通过 manual 标签排除它。这是组件测试，不是 T16 组合认证；Chromium、JuiceFS/S3、恢复及完整运行矩阵仍为 `not_run`。

## 12.4 部署依据

依据上游 [gVisor 安装指南](https://gvisor.dev/docs/user_guide/install/)、[containerd 接入](https://gvisor.dev/docs/user_guide/containerd/quick_start/) 与 [shim 配置](https://gvisor.dev/docs/user_guide/containerd/configuration/)，固定并校验发行产物，包括完整 gVisor 归档。containerd 2 使用 version 3 runtime 表；K3s 按[高级配置](https://docs.k3s.io/advanced)扩展 base 模板。显式配置 runsc `systrap` 平台；组件测试失败时不能替换为 runc 或放开 privileged。
