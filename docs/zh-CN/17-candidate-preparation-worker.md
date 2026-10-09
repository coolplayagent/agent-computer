# 17. Candidate 持久化准备 worker

## 17.1 输入权威与迁移

迁移 8 新增不可变 Workspace 输入版本、启动请求到输入的绑定，以及持久化 Candidate 准备记录。通过 apply 新建 Workspace 时，明确记录版本 1 的空 manifest。启动准入在分配 generation、Candidate 和容量预留的同一事务中固定该输入版本。缺少输入属于错误，不会触发文件重置。

迁移不自动填充旧 Workspace 或旧排队请求。运维明确确认某个旧 Workspace 应从空状态开始后，可通过受信任的数据库管理命令初始化：

```bash
agent-computer-server workspace-initialize-empty \
  --database-url-file /private/control/database-url \
  --organization org_example --workspace-id res_actual_workspace
```

该命令仅创建缺失的输入 head，不覆盖已有版本。重复调用返回已有版本，不重复发事件。旧请求已处于 Preparing/Prepared 时拒绝初始化。初始化后需取消并重新提交旧排队请求，不追溯修改旧请求缺失的输入绑定。

产品代码目前只发布空初始输入。非空 Artifact 发布、对象存储读取授权和缓存填充仍未实现；worker 不接受租户提供的 manifest 或摘要作为输入权威。存储组件独立的非空文件测试不能证明这些产品服务已交付。

## 17.2 认领、派发与完成

受信任 worker 处理指定准入请求和运维绑定的本地 Volume，在认领、派发、完成时重查原始 credential ID、主体启用状态、准确 scope、全部运行 grant 和固定目录依赖。新凭据不能接管原请求。成功的 Volume 协调记录必须匹配固定 Volume 版本/摘要以及不可变 PVC/PV UID 记录，包含 namespace UID。

首次认领固定文件系统 UUID、Volume 路径、写入 UID/GID、准备请求与摘要，重试改变绑定即失败。单 worker 持有 180 秒数据库租约，竞争者得到 `busy`。未派发前过期允许再次执行认领；复制前必须持久记录派发，将请求从 `Queued` 改为 `Preparing`，递增控制版本，并重新检查队列截止时间。取消可在这一转换之前获胜，不能释放 Preparing 或 Prepared 请求。

派发之后租约过期或回执丢失，接管者**只能观察**。`observe_prepared` 读取原最终目录，核对收据与 inode，重新确认配额并同步元数据；不会创建暂存目录、重新复制输入或重置已修改数据。发布目录缺失或存储错误将请求保留为 Preparing，原因为 `storage_unknown`，资源预留继续保留。后续观察可以发现原派发最终完成的发布；没有自动清理、另分 generation 或物理隔离声明。

有效存储收据绑定请求摘要、文件系统 UUID、PVC UID、准确数据路径、inode、manifest 摘要与配额。数据库原子提交收据、`Prepared` 状态、控制版本、事件与 Outbox。精确完成重试幂等，旧租约不能覆盖结果。公开运行状态查询新增 `Prepared`，但 `ready` 仍为 false。Prepared 仅表示受信任适配器已确认文件和配额；Pod 创建、写入租约、驱动健康及 Computer Ready 是后续工作。

## 17.3 本地 worker 部署

运维提供经核验的完整 JuiceFS 1.4.1 挂载，使用 PostgreSQL 元数据和 S3 数据、禁用 writeback，并预先准备与已记录 CSI 卷对应的私有 Volume 目录。后续应用只能挂载准备后的 data 叶目录。namespace/PVC/PV ID 会与日志核对；真实本地挂载、路径和文件系统 UUID 到该 Volume 的映射仍由运维部署负责。root 权限、配置及路径祖先必须受信任。

```bash
agent-computer-server candidate-prepare-once \
  --database-url-file /private/control/database-url \
  --organization org_example --worker-id storage_worker \
  --request-id start_actual_request \
  --config-file /private/control/candidate-worker.json
```

以下为配置示例，需替换实际 ID 与可执行文件摘要：

```json
{
  "target": {
    "volume_id": "res_actual_volume",
    "namespace_uid": "actual_namespace_uid",
    "pvc_uid": "actual_pvc_uid",
    "pv_uid": "actual_pv_uid",
    "filesystem_uuid": "actual_filesystem_uuid",
    "volume_path": "actual_csi_volume_directory",
    "writer_uid": 1000,
    "writer_gid": 1000
  },
  "mount_root": "/srv/juicefs",
  "object_cache": "/private/authorized-objects",
  "quota": {
    "executable": "/usr/local/bin/juicefs",
    "executable_sha256": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    "metadata_url": "postgres://juicefs_meta@metadata.internal/juicefs?sslmode=verify-full",
    "password_file": "/private/juicefs-password",
    "timeout_seconds": 30
  }
}
```

配置、数据库 URL 与密码文件继续遵循私有文件规则。控制库和 JuiceFS 元数据库使用独立数据库、角色与凭据。命令返回 `"busy"`、`"prepared"` 或 `"storage_unknown"`。数据库失败可能意味着提交结果未知，应保留原请求身份。没有开放 HTTP worker 端点，也不接受租户提供的 worker handle。

文件系统操作在阻塞工作线程执行。缓慢或卡住的 FUSE 操作可能超过协调租约时间；租约过期不会杀死它或证明它已停止。该旧租约不能提交迟到的完成结果。资源预留与私有暂存保留，等待权威观察或后续带隔离证明的清理协议。当前是单请求编排，不是公平队列调度器、持续租约 watchdog 或生产部署认证。

## 17.4 验证

新增 7 项真实 PostgreSQL 测试覆盖输入/Volume 绑定、不隐式重置的迁移、凭据/grant 重查、旧认领、仅观察接管、取消、错误收据、原子回滚与 WAL 重启。新增 2 项存储测试验证观察缺失目录不产生副作用、保留编辑与绑定校验。默认 Cargo/Bazel 测试合计 178 项；真实组件测试单独执行。

显式目标 `//crates/worker:candidate_worker_live_test` 要求一次性、root 管理的 Linux 环境，包括真实 Kubernetes/CSI、完整 JuiceFS 挂载、分离的控制/元数据 PostgreSQL 数据库及 S3。`AGENT_COMPUTER_CANDIDATE_TEST_CONFIG` 指向私有测试配置。测试供应真实 Volume，调用运维命令，核验发布后的 inode 与所有者，在实际发布后注入数据库回执丢失，并验证观察不会重新复制。另一个已派发但无发布目录的请求必须保持未知且不创建文件。详见[测试源码](../../crates/worker/tests/candidate_live.rs)；仅构建该手动目标不算执行证据。

产品 Pod 挂载、fencing、非空 Artifact 输入、stop/recover、清理及 T01–T43 验收仍待完成。
