# 38. Workspace Artifact 与纯文件 checkpoint

## 38.1 发布固定文件版本

所有有界文件写者均释放后，`POST /v1alpha1/workspaces/{id}/artifacts` 永久封存当前 Candidate。请求需要幂等键、原 start 的固定输入身份及 Computer 当前控制修订：

```json
{
  "request_id": "start_example",
  "expected_revision": 4,
  "base_revision": 1,
  "base_manifest": "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
  "publish_current": true
}
```

服务凭据需要 `runtime.publish`、`runtime.read` 和 `runtime.modify`；资源授权需要 Workspace read/modify/publish 与 Computer read/modify。正式发布时再次检查。`202` 返回 Artifact 的 `commit_id` 和 `Capturing` 状态，表示持久化工作已接收，不表示发布成功。`GET /v1alpha1/artifacts/{id}` 查询当前元数据；`/manifest` 在发布前返回 null，发布后返回固定相对文件清单。两种读取都检查当前 Workspace read 权限及 `runtime.read`，不返回 S3 凭据、对象键或签名地址。

迁移 21 通过组织事务锁串行化封存与写者抢占。`Prepared` 进入 `Sealing` 后不可恢复写入。所有写者 epoch 必须 Released；历史派发必须具有已关闭的有界文件完成记录及 `bounded_file_drained` 证明。排队或已派发进程、未知 IO、未释放租约都会阻止封存。进程退出、cgroup 消失、Pod 删除及租约到期均不能证明 JuiceFS 排空。

## 38.2 捕获、上传与重试

受信操作端运行：

```sh
agent-computer-server artifact-publish-once \
  --database-url-file /private/control-url \
  --organization example --worker-id publisher \
  --commit-id artifact_example --config-file /private/artifact-worker.json
```

私有 JSON 配置包含 `storage`（现有文件 worker 的 `target` 与 `mount_root`）、`spool`（已存在的私有目录）及 `objects`（[36](36-durable-execution-outputs.md) 的 S3 配置）。S3 使用独立 `artifacts/v1` 前缀，绑定组织、提交身份和内容哈希。凭据及 spool 必须位于工作负载挂载之外；存储目标必须与 Candidate 准备时的持久化身份精确一致。

捕获器沿打开的目录和文件描述符遍历，不跟随链接、不跨挂载。支持普通文件、目录、空目录和 executable 位；拒绝符号链接、硬链接、特殊文件、文件特殊权限位及遗留的文件保存暂存名。遍历前后检查身份、长度、时间戳和目录项。文件 fsync 后按最多 4 MiB 的分块读取，同时记录完整文件 SHA-256。分块及清单先持久化到 spool，再记录捕获身份。清单仍限 10,000 项及对象客户端规定的清单大小；Candidate 仍预留 10 GiB。本分块格式不代表通过 S3 multipart-upload API 认证。

worker 上传不可变对象后完整读回所有分块及清单，校验长度、分块哈希及完整文件哈希；执行中续租 300 秒的上传租约。最终事务重新检查租约、控制修订、generation、发布者凭据、资源授权及固定目录依赖；event/outbox 写入后再检查租约与凭据。新输入版本、可选默认指针、Artifact 结果、`Sealed` 状态和 event/outbox 同事务提交。

捕获、上传或数据库失败均保留已封存 Candidate 与已完成的 spool 对象。可重跑同一操作端命令，已记录的捕获不会被静默替换。失败 worker 只释放自己的租约；进程中断后可等待租约到期再接管。发布者凭据过期时，原 principal 使用重新授权的凭据、原幂等键及完全相同输入重复 POST，可废止旧 worker 权限而不修改捕获内容。任何错误路径都不会恢复 Candidate 写权限。

`publish_current=true` 对 Workspace 当前 head 与 `base_revision` 做 CAS。匹配则推进 head；冲突则保留独立 Artifact/输入版本及 Candidate，返回 `Conflict`，不覆盖其他发布者的 head。`false` 创建分支版本并保持 head。分支目前继承 Workspace read ACL；尚未提供分支继续编辑、显式 fork/rebase、私有逐 Artifact ACL 或冲突解决入口，因此这些封存 Candidate 继续保留。

## 38.3 checkpoint 停止与恢复

成功发布为当前 head 后，原有[停止接口](37-undispatched-computer-stop.md) 接受 `Sealed` 的纯文件 Computer。`artifact_checkpoint` 回执绑定 Artifact、输入修订及清单哈希、Computer spec 哈希和原运行快照哈希；App 状态与未结束执行列表为空。声明任何 App 的 Computer 必须等待所需 App/profile 捕获实现后才能走此 checkpoint 路径。分支、冲突、已被替换的 Workspace head 及活跃人类输入同样阻止 checkpoint 停止。

停止保留旧 Candidate 及完整存储预留。后续普通 start 创建新 generation 和 Candidate，并固定 Workspace 当前输入。Candidate worker 可选的 `artifacts` 配置提供同一个 S3 存储；恢复时校验远端分块和清单，构建私有内容哈希缓存，再实体化独立可写文件并复验完整哈希，不复制旧 Candidate。已有准备回执或已派发准备的观察重试无需再次下载对象。容量必须同时覆盖旧 Candidate 与新 Candidate；垃圾回收待实现。

本增量也修正了此前停止回执的事件序号，使其等于实际提交的 `computer.stopped` 事件。Computer `ready` 仍为 false。任意进程存储 fencing、通用/强制停止、App/浏览器状态、checkpoint driver 版本协商、Artifact GC、Presentation 及 T01–T43 完整产品验收仍待实现。

## 38.4 验证

PostgreSQL/HTTP 测试覆盖封存与抢占竞态、未知派发、历史不可变性、分支/CAS 冲突、即时授权、凭据替换、outbox 写入期间租约到期、事务回滚、迁移、checkpoint 停止、WAL 重启及新输入准入。文件系统测试覆盖多分块文件、独立 inode、空目录、模式保留、损坏和并发修改。使用合成存储回执的数据库用例仅证明授权与事务边界。

真实一次性单 VM 实验使用 K3s/CSI、JuiceFS、分离的控制/元数据 PostgreSQL 数据库及 SeaweedFS S3，通过有界文件网关保存文件，拒绝错误 S3 凭据，注入发布 outbox 失败，删除本地 spool 后由新进程恢复发布，再 checkpoint 停止。清空缓存并故意修改旧目录后，新 Candidate 仍从 S3 恢复原内容、executable 权限及独立 inode。这是组件证据，不是多节点 fencing、断电、HA 或完整 Computer 认证。交付证据记录源码哈希、独立 S3 读回与环境清理。
