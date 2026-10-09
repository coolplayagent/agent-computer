# 16. Computer 持久化启动准入

## 16.1 已交付范围

迁移 7 新增持久化启动请求和 Computer 控制计数器。服务可接收启动请求并保存为 `Queued`，在响应丢失后返回原始回执，查询当前准入状态，并取消尚未派发的请求。能力 `computer.start_admission` 为 `control-plane`；`computer` 仍为 `unsupported`。

后续 [17 Candidate 准备 worker](17-candidate-preparation-worker.md) 已在准入时绑定已提交 Workspace 输入，并提供独立的授权存储 worker。新 Workspace 创建时记录明确的空初始输入；已有 Workspace 缺少输入时不会默认为空。非空 Artifact 发布、写入租约、Pod 派发、隔离与驱动健康仍待实现。启动准入本身不准备文件，也不报告 Ready。

## 16.2 原子准入

`POST /v1alpha1/computers/{id}/start` 要求服务 Bearer 凭据、单个 `Idempotency-Key` 请求头，以及未压缩的 `application/json`：

```json
{
  "expected_revision": 1,
  "expected_spec_revision": 1,
  "max_runtime_seconds": 300
}
```

`expected_revision` 为 Computer 控制版本，初始为 1；`expected_spec_revision` 指定当前 Computer 定义版本。generation 初始为 0，首次成功准入分配 generation 1、控制版本 2。这三个计数器相互独立。取消只递增控制版本，不重置 generation；再次启动会分配新的 generation 和 Candidate ID。ID、组织与主体均由服务端确定；请求不能附加 generation、Candidate、checkpoint 或 actor 等字段。

一个 PostgreSQL 事务持有组织事件流锁，重新校验凭据/主体，检查完整运行权限集，固定不可变资源图，检查容量，并写入请求、计数器、预留量、幂等回执、事件与 Outbox。提交前再次检查凭据是否过期。失败将整体回滚。凭据与资源授权撤销遵循已有的[授权锁定契约](15-runtime-authorization.md)。

资源图保存准确的资源版本、摘要、规格与传递依赖。非目录资源的当前版本更新不会改写已捕获版本；目录引用必须仍启用且版本/摘要一致。同一资源出现冲突版本时拒绝准入。图限制为 256 个资源、1 MiB、32 个不同授权要求。快照不保存 Bearer token；请求另外绑定原始 credential ID。相同主体换用有效凭据重放不会转移这一绑定。

所需权限为 Computer `activate`；主 Workspace `read` 与 `modify`；其他引用 Workspace 的 `read`；每个 App 的 `app.use` 与限时 `activate`；每个私有浏览器 profile 的 `app.use`。各项同时要求相应凭据 scope。定义 manage/reference 权限不产生运行授权；此接口也不授权任意 shell 执行。

## 16.3 容量预留与队列上限

以下保守平台上限在数据库事务内执行，调用者不能覆盖：

| 预留项目 | 上限 |
| --- | --- |
| 每组织未结束请求 | 64 |
| 组织内每主体未结束请求 | 8 |
| 每 Computer / 可写 Workspace 未结束请求 | 1 / 1 |
| 每组织 CPU 预留 | 64,000 millicores |
| 每组织内存预留 | 131,072 MiB |
| 每组织 Candidate 存储预留 | 1 TiB |
| 每请求 Candidate 存储 | 10 GiB |
| 每 Volume Candidate 存储预留 | 固定 Volume 的 `quotaBytes` |
| 每主体请求运行时长预留总和 | 86,400 秒 |

每请求汇总不同固定 Sandbox 的资源量。`Queued`、`Preparing` 和 `Prepared` 均占用预留。容量不足的 Volume 会被拒绝，不对声明取整或扩容。容量不足返回 429，且不创建请求。这些预留是准入账目，不代表实际 Kubernetes 调度、文件系统用量或计费。公平调度、运维预算配置和历史累计计费仍待实现。

数据库分配 15 分钟队列截止时间。未来 dispatcher 必须拒绝过期请求，并在任何副作用前重新校验原始凭据、授权、预算与输入。目前没有自动队列过期 worker，可通过取消释放未派发请求的预留。截止时间或凭据撤销均不能释放可能已派发的写入者。运行时长是后续执行必须落实的限制；准入本身尚未启动运行时 watchdog。

## 16.4 回执、查询与取消

成功返回 202，包含稳定的 `request_id`、`candidate_id`、generation、控制/规格版本、快照摘要、输入版本/manifest 摘要、资源预留、截止时间和事件序号。旧回执不含输入字段。状态为 `Queued`，原因为 `awaiting_runtime_preparation`。相同键与输入的重试重新校验当前授权，并返回原始回执；不同输入返回 409，已退休键返回 410。其他键不能分配第二个活动 generation。取消后的重放仍返回原始准入回执，不会重新激活请求。

`GET /v1alpha1/computers/{id}/runtime` 要求 Computer `read` 和 `runtime.read`，返回当前版本、generation、活动请求 ID 与启动状态；本阶段 `ready` 始终为 false。未启动的 Computer 查询为版本 1、generation 0，不创建运行记录。响应不披露私有依赖图或 credential ID。

`POST /v1alpha1/computers/{id}/start/cancel` 要求 Computer `manage`、`runtime.manage`、幂等键与以下请求体：

```json
{
  "expected_revision": 2,
  "request_id": "start_actual_request"
}
```

取消必须指定当前控制版本对应的活动请求。事务只能把 `Queued` 改为 `Cancelled`，同时释放预留、递增控制版本并发布事件/Outbox。请求身份、快照与 generation 历史保持不可变。`Preparing` 返回 409 并保留预留，隔离与清理需要单独的 worker 协议。此接口不是 Computer stop/recover。三个接口都拒绝浏览器 Origin；资源不存在与无权限统一返回 404，无效凭据为 401，缺少 scope 或超过授权时长为 403，过期定义版本为 412。详见 [OpenAPI 契约](../../schemas/openapi-v1alpha1.json)。

## 16.5 验证与未完成工作

11 项真实 PostgreSQL 测试覆盖迁移保留、并发重试、不可变快照、授权/scope 隔离、目录禁用、等待准入时撤销、Volume/Workspace 容量竞争、主体队列上限、回执/Outbox 失败回滚、提交前凭据过期、WAL 重启恢复，以及准备开始后拒绝取消。1 项 HTTP 测试覆盖解析、服务端身份字段、版本、重放、查询、取消、scope 与 Origin 拒绝。这些属于组件契约，T01–T43 仍为 `not_run`。

后续 [17 准备 worker](17-candidate-preparation-worker.md) 已实现已提交输入绑定、Volume 身份核验及持久化准备认领/收据。下一步需发布并授权读取非空 Artifact 输入，落实写入租约，并接入 Pod/驱动观测。物理隔离、stop/recover、checkpoint、租约 watchdog 和完整运行时验收仍未完成。
