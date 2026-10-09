# 13. 持久化 JuiceFS 卷供应

## 13.1 已交付范围

`agent-computer-worker` 将已准入的 Volume 意图连接到 Kubernetes HTTPS 适配器。下文运维命令每次认领一个任务后退出，可由外部进程重复调用。它只认领 Volume 意图，并保留依赖顺序；其他资源类型仍待协调。发布 Computer 或 Sandbox 声明不会启动 Pod。

首条路径支持第 1 版、`Retain`、显式注册并授权的 StorageClass ID，以及整 GiB 配额。扩容、删除、配额变更和非整 GiB 供应尚不支持。静态声明仍能表达字节配额；worker 明确报告后端限制，不会自动向上取整。

运维配置固定 namespace、RuntimeClass、拒绝全部流量策略、StorageClass 和 CSIDriver 的 UID。预检要求合格的 `csi.juicefs.com` 驱动、`Immediate` 绑定、`Retain`、`writeback=false` 和精确的供应/node-publish Secret 引用。控制器无权读取这些 Secret，存储凭据保留在受信任的 CSI 部署中。

## 13.2 派发与观察

每次认领持有数据库租约，并重新检查原始发起者当前的权限。派发准入先提交，再执行一次 PVC POST。PVC 名称由组织及稳定 Volume 身份派生，注解保存完整准入规格和部署绑定。创建冲突或回执丢失后读取同一名称；worker 不分配第二个名称，也不在派发后发现对象缺失时自动重建。

迁移 5 增加不可变的协调对象记录。首次观察 PVC 时立即保存 UID，包括 Pending 状态；后续认领必须观察同一 UID。namespace、StorageClass、驱动、PVC 或已知 PV 被替换都会阻断进度。写入和精确重试遵守租约、权限、事件及 Outbox 的事务约束。重试延迟两秒；不确定副作用在租约过期后仍只能观察。

Bound PVC 必须与 PV 的 claim UID/名称/namespace、容量、保留策略、驱动、文件系统、同步挂载选项、Secret 引用及每 PV 子目录相互吻合。额外 CSI 参数和其他卷源会被拒绝。两个对象 UID 均记录后才能完成。回执标识 PVC 与 PV，仅表示供应事实。

**PVC/PV Bound 不代表 Workspace 可用或目录配额已生效。** 固定的 JuiceFS CSI v0.33.0 控制器在返回 CreateVolume 前异步安排配额设置；即使 `juicefs/controller-quota-set` 属性存在，也不能证明设置完成。Candidate 准备仍需独立验证配额、访问、flush 行为及 generation 归属，之后才允许 writer。产品 Sandbox 尚不支持直接挂载整个 Volume。物理 fencing、多节点恢复、快照、Artifact 与垃圾回收仍待实现。

## 13.3 运维配置

先迁移数据库、签发发起者凭据、授予 Volume 创建与 StorageClass 引用权限、注册目录引用，再按[10 声明计划与原子 apply](10-plans-and-apply.md)发布声明。worker 配置必须使用该目录资源 ID 及真实集群 UID。这是受信任运维接口，不是租户 API。

```bash
agent-computer-server reconciliation-volumes-once \
  --database-url-file /private/control/database-url \
  --organization org_example --worker-id worker_1 \
  --config-file /private/control/volume-worker.json
```

JSON 示例，部署占位符均需替换为实际值：

```json
{
  "api_url": "https://127.0.0.1:16443/",
  "ca_file": "/private/control/ca.pem",
  "token_file": "/private/control/kubernetes-token",
  "deployment": {
    "namespace": "ac-kube-component",
    "namespace_uid": "actual-namespace-uid",
    "runtime_class_uid": "actual-runtimeclass-uid",
    "deny_policy_uid": "actual-networkpolicy-uid",
    "network_policy_ref": "deny-all"
  },
  "storage": {
    "reference": "id:actual-catalog-resource-id",
    "name": "ac-juicefs",
    "uid": "actual-storageclass-uid",
    "driver_uid": "actual-csidriver-uid",
    "secret_name": "ac-juicefs-secret",
    "secret_namespace": "kube-system"
  }
}
```

配置、数据库 URL、CA 和 token 必须是私有普通文件，最大 8 KiB。Kubernetes 客户端只使用显式 CA、HTTPS origin 和有时限 token，不隐式发现 kubeconfig，不启用重定向、代理或 HTTP 自动重试。运维轮换凭据时保留固定部署身份。JSON 结果表示 idle 或持久化意图进度；进程成功退出不等于意图成功，需读取 Blocked/重试原因及 operation 接口。

## 13.4 验证与部署依据

数据库测试覆盖 UID 记录的不可变/幂等行为，记录/事件/Outbox 一起回滚，撤权、陈旧租约，以及保持依赖顺序的类型筛选认领。四项卷协议测试另外覆盖部署身份变化、破坏性回收、异步上传选项、其他数据源、PVC/PV 双向绑定、额外后端参数、容量溢出及创建确认丢失后不重复 POST。

显式实测需要一次性集群：[Kubernetes 测试 namespace](../../deploy/testing/kubernetes-component.yaml)、运维安装的固定版本 JuiceFS CSI、独立格式化且使用 PostgreSQL 元数据与 S3 数据的文件系统，最后配置[卷 RBAC/StorageClass 测试清单](../../deploy/testing/juicefs-component-rbac.yaml)。该清单只授予 PVC get/create 及 PV/StorageClass/CSIDriver 读取权，无删除卷或读取 Secret 权限。受信任 CSI 节点组件需要测试集群内的特权；应用 Pod 仍须使用受限 gVisor 配置。

将上述私有配置设为 `AGENT_COMPUTER_VOLUME_TEST_CONFIG`。实测会替换为自己的临时目录 ID，并可额外接收 `evidence_file` 输出无凭据 UID 记录；运维命令明确拒绝这个测试专用字段。PostgreSQL 测试前提见[08 持久化](08-persistence.md)。

```bash
bazel test //crates/worker:volume_worker_live_test \
  --test_env=AGENT_COMPUTER_VOLUME_TEST_CONFIG=/private/component/volume-config.json
AGENT_COMPUTER_VOLUME_TEST_CONFIG=/private/component/volume-config.json \
  cargo test -p agent-computer-worker --features live-test --test live --locked
```

显式目标缺少配置就失败，默认测试不执行它。它验证真实授权 plan/apply、worker 派发和 CSI 供应，检查持久化对象记录及后续 worker 空闲。PVC/PV 一直保留到隔离环境整体销毁。文件持久化及实际配额另需探针，此测试本身不认证 T16 或 T19。

上游依据包括[动态供应指南](https://juicefs.com/docs/csi/guide/pv/)、[CSI 安装](https://juicefs.com/docs/csi/getting_started/)、[PostgreSQL 元数据指南](https://juicefs.com/docs/community/databases_for_metadata/)，以及固定的 [v0.33.0 控制器实现](https://github.com/juicedata/juicefs-csi-driver/blob/v0.33.0/pkg/driver/controller.go)。生产部署仍需通过 CodeSpec 的部署与恢复认证。
