# 27. 执行 Pod 的 Candidate 数据挂载

## 27.1 已交付行为

`CandidateMount` 与 `StartupSandboxPlan::with_candidate` 将 [26 启动通道](26-kubernetes-startup-attach.md)扩展到已准备的 JuiceFS Candidate。计划只把固定的 `organization/{org}/workspace/{workspace}/candidates/{candidate}/generation/{generation}/data` 叶目录挂载到 `/workspace`；卷根目录、其他 Candidate 和准备回执不暴露给应用。原构造器继续支持临时工作目录。

构造器接收已限定的 Volume 计划、namespace/PV UID、CSI 卷路径、原 `PrepareRequest` 与 `Prepared` 回执，核对固定写入 UID/GID 1000 的准备摘要、原始清单摘要、配额、PVC UID、数据路径、generation 及非零 inode。Volume 的组织必须一致，配额必须覆盖 Candidate。Sandbox 声明必须且只能包含该 Workspace `id:` 引用到 `/workspace` 的可写挂载，不能覆盖路径、增加工作区或隐式挂载；Pod 身份必须匹配准备请求的组织、Computer 与 generation。

这是可信控制器接口。准备回执是存储证据，不是写租约；调用方仍须核验真实可信文件系统挂载/回执，并在运行前取得当前派发和启动授权。租户 HTTP 输入不会获得该构造能力或任意 PodSpec 权限。

## 27.2 回读与权限

创建 Pod 前、attach 前、收到挑战后及发送授权前，客户端都会重读部署、StorageClass、CSI driver、PVC 和 PV，校验固定 UID、双向 claim 绑定、同步 `writeback=false`、保留策略与限定的 CSI secret 引用；要求 PV 名称、CSI handle 和供应子目录均匹配固定卷路径。旧对象或替换对象会阻止下一次变更。Pod 不可变注解记录紧凑存储绑定与回执，不嵌入完整输入清单或存储秘密。

Pod 不配置 `fsGroup`：已准备数据归 UID/GID 1000，卷根目录、generation 父目录和回执仍归可信运维所有。Kubernetes/CSI 可能根据 `fsGroup` 修改整个挂载卷的权限，见 [Kubernetes 安全上下文](https://kubernetes.io/docs/tasks/configure-pod-container/security-context/)。严格准入回读拒绝注入 `fsGroup`、supplemental groups、替换 claim/subPath、挂载表达式或额外卷；仅在默认值确为 false 的两种字段上容许省略 `readOnly=false`。

gVisor bind mount 必须保留 shared 语义，不能为有外部观察者的工作区启用 exclusive 缓存。适配器不接受计划外运行注解；真实节点配置仍须由运维认证，见 [gVisor 文件系统](https://gvisor.dev/docs/user_guide/filesystem/)。

Kubernetes 使用 PVC 名称挂载，不能以 UID 对挂载执行原子前置检查。因此这些检查依赖运维独占 RBAC、可信 CSI/节点配置及私有不可替换的 Candidate 父路径。API 回读不能单独证明实际挂载 inode、文件系统 UUID、配额执行或物理 fencing。存储不可用时仍可按 Pod UID/resourceVersion 条件删除，但删除不构成排空或完成回执。

## 27.3 验证与复现

新增 7 项协议测试覆盖准备绑定错配、挂载范围、准入修改、创建前存储变化、attach 三处身份替换、一次性成功通道及不依赖存储可用性的删除。默认工作区共 278 项测试，其中 Kubernetes 33 项；Cargo/Bazel、fmt/Clippy 和文档检查独立于现有行尾 Qualitygate 策略执行。

手动目标 `//crates/kubernetes:candidate_mount_live_test` 通过真实 CSI 供应保留卷，准备两个独立 Candidate，经 v5 attach 启动可信监督器，核对原始文件、写入并 fsync 各自文件，随后重新验证准备回执和目录所有权/模式。它在临时 VM 中作为可信 root 控制器运行，应用仍以 UID/GID 1000 在 gVisor 内执行。授权来自协议夹具，不能证明数据库执行派发、持久结果接纳或物理 fencing。

```sh
bazel build //crates/kubernetes:candidate_mount_live_test //crates/sandbox:agent-computer-sandbox --lockfile_mode=error
python3 crates/sandbox/tests/rootfs.py --supervisor bazel-bin/crates/sandbox/agent-computer-sandbox --destination /tmp/ac-candidate-rootfs
python3 crates/kubernetes/tests/startup_image.py --rootfs /tmp/ac-candidate-rootfs --output /tmp/ac-candidate-image.tar
```

将摘要镜像导入临时集群。使用 [17 Candidate 准备](17-candidate-preparation-worker.md)中的已限定存储环境及私有 `kubernetes`/`candidate` 配置，包含可信 JuiceFS 全量挂载、已验证 UUID、对象缓存及配额执行器。部署 `network_policy_ref` 设为 `deny-all`，顶层增加 `image` 与 `result_file`，应用 [attach 测试 RBAC](../../deploy/testing/startup-attach-rbac.yaml)。可选 `observation_file` 会在 attach 前等待同目录 `.inspected` 标记，最长 15 秒，供外部节点检查。

```sh
sudo env AGENT_COMPUTER_CANDIDATE_MOUNT_CONFIG=/private/candidate-mount-config.json /path/to/candidate_mount_live_test --nocapture
```

测试按身份条件删除执行 Pod，并保留 Volume 供独立读取；收集证据后再回收私有 VM。产品监督器打包、数据库派发 worker 接入、独立 watchdog、物理 fencing、有界输出对象及权威完成仍待实现；运行验收 T01–T43 保持 `not_run`。

显式实测通过两个 Candidate 场景。节点检查将实际 kubelet subPath 绑定 inode、UID/GID 1000 与准备回执匹配；新的只读 JuiceFS 客户端无磁盘缓存读到两个 fsync 文件，并记录两次 S3 GET。卷根目录、generation 父目录、回执及数据所有权/模式保持不变。首次测试因 CSI 辅助镜像下载超时，第二次停在节点检查脚本的错误路径假设；修正检查脚本后，同一测试二进制在第三次通过，前两次失败记录随证据保留。

[固定源码记录](../evidence/candidate-pod-mounts-2026-10-10.json)和[测试原始输出](../evidence/candidate-pod-mounts-2026-10-10.log)绑定 `51600f9`、203 项已核验源码/构建输入、精确二进制/镜像、节点 inode 检查及独立 S3 读取，并明确保留两次失败尝试。收集后已回收本次 VM、私有存储和凭据。
