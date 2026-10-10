# 39. 并行 Candidate 与 Artifact 继续编辑

不同 Computer 现在可以在同一 Workspace 中准备、编辑各自独立的 Candidate。每个 Computer 仍只有一个活动 generation，每个 Candidate 分别保留写入租约、目录、配额和存储预留。发布仍使用 Workspace head 的比较交换规则；并行准入不会共用可写目录，也不会覆盖另一发布者的成果。

## 选择固定起点

`POST /v1alpha1/computers/{id}/start` 新增可选 `input_artifact_id`：

```json
{
  "expected_revision": 5,
  "expected_spec_revision": 1,
  "max_runtime_seconds": 300,
  "input_artifact_id": "artifact_example"
}
```

实际调用应使用 stop/runtime 回执中的控制修订号。Artifact 必须已在该 Computer 所属 Workspace 内完成发布，状态为 `Committed` 或 `Conflict`。CAS 冲突仍保留有效的不可变数据，只表示另一发布者推进了共享 head。未完成、不存在、其他组织或其他 Workspace 的 Artifact 均不可选择。选择时需要完整的当前运行依赖图授权，包括 Workspace read/modify grant 及对应凭据 scope；知道 Artifact ID、哈希或对象路径不会获得权限。

省略字段或传 null 时，选择 Workspace 当前 head。只要输入来自 Artifact，不可变启动回执就包含 `input_artifact_id`，默认选择也如此；初始空输入与历史回执不含该字段。相同请求重试保持原固定版本并重新验证授权，同一幂等键更换选择器会冲突。准入不会改变 Workspace head。

迁移 22 移除 Workspace 活动请求唯一约束，保留 Computer 唯一约束和全部容量上限。新的输入绑定必须与准入回执中的 Workspace、修订、清单摘要和 Artifact 来源一致；历史数据保持不变。写入事务事件、outbox 和回执后再次检查完整依赖图授权，失败会回滚整次准入。

## 停止并继续分支或冲突版本

纯文件 [Artifact checkpoint](38-workspace-artifact-checkpoints.md) 现在允许停止在已提交分支、CAS 冲突或已经不再是 head 的发布版本上。回执绑定该 Computer 自己的固定 Artifact。原有限制继续有效：没有待捕获的声明 App、此前写者已获得可验证排空证据、没有已派发的进程执行、没有活动人类输入。本轮没有新增进程隔离证明。

继续同一 checkpoint 时，在 start 中传入它的 `checkpoint.artifact_id`。普通 start 跟随当前 Workspace head，可能选择另一版本。worker 从校验后的远端对象恢复出独立文件、新 Candidate 和新 generation；旧 Candidate 及存储预留继续保留。

例如两个 Candidate 从修订 1 开始，首个发布为修订 2，第二个保留冲突修订 3，随后两者均可停止。默认重启读取修订 2，显式选择则读取修订 3。后者继续编辑并发布分支得到修订 4，head 仍为 2。选择修订 3 或 4 不会把 CAS 基线重置为 head 2，直接发布为当前版本仍会冲突。明确解决内容冲突时，应从当前 head 2 准备 Candidate，应用选定的改动，再以该基线发布。真实实验完成了这种显式合并，将 head 推进至修订 5；本轮不引入自动合并或 rebase API。

## 验证与剩余工作

新增六项 PostgreSQL 契约覆盖 WAL 重启后的独立输入、选择器幂等性、新鲜授权、未发布及跨 Workspace 拒绝、输入绑定篡改、事务回滚和迁移保留。HTTP 契约覆盖选择器语法、不可用选择、null/默认等价和旧请求重试兼容。完整默认测试通过 360 项 Cargo 测试，其中 PostgreSQL 168 项、HTTP 22 项；11 个 Bazel 目标通过。

真实临时 K3s/CSI + JuiceFS + S3 实验运行同时存在的写租约与不同内容保存，拒绝跨 Candidate 权限借用，保留两个实际发布结果，分别恢复默认与冲突版本，继续编辑并恢复分支，最后从当前 head 显式合并。内容和 inode 检查证明文件独立。独立 Python SigV4 客户端从 S3 读取全部五个 Artifact 清单与六个分块对象，校验七个完整文件，其中包含原有空文件场景。这是单 VM 组件证据；数据库构造数据本身不证明存储行为。

[绑定源码的交付证据](../evidence/artifact-candidate-continuation-2026-10-10.json)与[日志](../evidence/artifact-candidate-continuation-2026-10-10.log)包含最终二进制哈希、独立 S3 回读以及已完成的 VM/临时凭据清理。Computer `ready` 仍为 false，T01–T43 产品验收保持 `not_run`。通用进程/CSI 排空、强制停止、App/browser/profile checkpoint、跨 Workspace fork、私有 Artifact ACL、自动冲突解决、Presentation 和垃圾回收仍待实现。保留的旧 Candidate 与新 Candidate 均占用容量。
