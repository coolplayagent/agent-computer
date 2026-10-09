# 23. 持久化执行准入

## 23.1 已交付行为

迁移 12 与认证 HTTP API 可在 Prepared Candidate 上持久保存连接存续期内的命令队列。提交在同一事务中预留当前写租约 epoch，排斥竞争的文件派发，固定输入并写入事件/Outbox。运行派发器尚未接入：能力返回 `execution.admission: connection-queued`、`execution: unsupported`；当前所有执行均为 `dispatch_started: false`。

本增量显式支持 `lifetime: connection`。原连接关闭、授权丢失、当前 Candidate 改变或固定排队期限到达后，查询/对账或释放写租约会取消未派发的预留。后台执行需要独立的执行生命周期/租约，仍待实现。HTTP 连接断开本身不会关闭 ConnectionSession 或撤销已提交准入；使用相同幂等键和输入重试。

## 23.2 HTTP 契约

所有接口使用原服务凭据和 `runtime.connect`，仍不支持浏览器 Origin 认证。首次提交另检查 `runtime.read`/`runtime.modify`、本人 Active 连接能力、Computer connect/read/modify grant、Workspace read/modify grant，以及准确的写租约 generation/epoch/revision。连接关闭或资源 grant 丢失后，只要原凭据仍有效，即可查询元数据和取消；同一主体的其他凭据不能访问。返回视图不包含命令字节或存储路径。

| 接口 | 输入 | 结果 |
| --- | --- | --- |
| `POST /v1alpha1/computers/{id}/executions` | Idempotency-Key、SubmitExecution | 202 Queued；精确重试返回当前元数据，取消后为 200 |
| `GET /v1alpha1/executions/{id}` | 原凭据 | 200 元数据；将已失效/过期的 Queued 对账为持久 Cancelled |
| `POST /v1alpha1/executions/{id}/cancel` | Idempotency-Key、执行的 `expected_revision` | 200 Cancelled，不释放写租约 |

请求必须是不超过 64 KiB 的未压缩 JSON，拒绝未知/重复字段。例如：

```json
{
  "lease_id": "lease_1",
  "lease": {
    "connection_session_id": "connection_1",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "sandbox_id": "sandbox_1",
  "lifetime": "connection",
  "command": {
    "argv": ["/bin/sh", "-c", "printf hello > result.txt"],
    "cwd": "",
    "timeout_seconds": 10,
    "term_grace_ms": 100,
    "output_limit_bytes": 65536
  }
}
```

命令验证复用 [22 监督器](22-sandbox-supervisor.md)实现：绝对可执行路径、显式解释器、最多 128 参数/32 KiB、规范化相对 cwd、1–3600 秒超时、0–5000 毫秒 TERM 宽限和每流 0–1 MiB 保留输出。超时也不能超过固定的 Computer 启动预算。API 不接受租约预算、进程已停止标志、任意环境、stdin 引用或后台生命周期。这里仅检查 cwd 语法；真正的描述符/挂载约束由运行派发层执行。

## 23.3 绑定、预留与取消

选择的 Sandbox 必须由已准入 Computer 快照直接引用、使用 gVisor，且不能被同快照中的 App 使用。准入固定其不可变 revision/spec digest、镜像/资源/网络依赖，以及 Candidate 准备收据和 inode、实际存储绑定、已提交 Workspace 输入 revision。读取固定快照，不静默采用更新的声明；固定目录引用禁用/版本漂移仍会使授权失效。任何实际副作用前，未来适配器还必须检查运行挂载和安全配置兼容性。

每个写租约 epoch 只有一条执行记录。创建预留会增加写租约 revision，续租或释放前应重新读取租约。排队截止时间固定为准入时的写租约到期时间，重试和后续续租都不能延长；当前租约策略下最多 30 秒。已有派发拒绝执行排队；反之，Queued 预留会阻止公共/可信文件派发入口以及数据库派发 trigger。租约上的 `dispatch_recorded: false` 仅代表没有外部副作用意图，不代表未被预留。

用户取消检查执行 revision CAS，不释放或续期写租约；取消后可进行文件派发，但再次提交执行需要新写租约 epoch。写租约释放会在同一事务中先取消 Queued，再写零派发证明并释放所有权。到期/撤权对账只能降低排队权限；Cancelled 不会恢复为 Queued。即使已进入下一 epoch，原提交的精确重试仍返回原终态。事件只包含 ID、状态与摘要，不含命令字节；取消/Outbox 失败回滚整个事务。

SQL 迁移仅允许 Queued → Cancelled，保留不可变输入/绑定/历史，并防止排队、派发和排空证明互相绕过。升级保留既有写派发，不凭空创建执行记录或排空证据。Running 与完成状态需要另行实现持久派发并迁移。

## 23.4 验证与后续工作

新增 11 项真实 PostgreSQL 场景覆盖 WAL 恢复、精确重试、固定绑定、不可变记录、旧代次/外部 Sandbox 拒绝、原凭据隔离、撤权/连接关闭、排队期限、文件派发竞争、准入与取消回滚以及迁移 12。另有两项 HTTP 场景覆盖排队/查询/取消/重试和非法、后台、超大、浏览器请求。准备收据为模拟证据，这些测试只证明控制状态行为。

工作区默认测试现为 242 项，其中 PostgreSQL 104 项、HTTP 19 项。Cargo 测试、fmt、Clippy、Bazel 构建/测试、OpenAPI 元 Schema/本地引用和双语文档检查通过。现有 full Qualitygate 只检查换行，不建立运行验收；T01–T43 仍为 `not_run`。

仍需实现运行 Pod/Candidate 挂载绑定、外部 watchdog/fencing、实际派发、持久输出对象、权威完成接受、后台生命周期与 Unknown 对账。独立监督器的 JSON 报告不能授权进程或释放这里的租约。
