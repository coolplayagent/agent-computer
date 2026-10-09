# 18. 持久化逻辑连接会话

## 18.1 身份与有效期

迁移 9 为人和 Agent 主体增加持久化 ConnectionSession。会话绑定稳定 Computer ID、认证得到的组织/主体以及创建它的确切服务凭据。多个主体可连接同一 Computer；同一主体可为多台 Computer 建立独立连接。连接不会创建运行控制行、generation、存储准备、Pod、租约或 ViewerSession。断开保留 Computer 及其后台工作。

请求有效期默认 900 秒，范围为 1–3600 秒。数据库取该期限与原凭据到期时间中的较早者。心跳不延长任一期限。计算代次变化时保留逻辑连接；后续执行仍须独立校验当前代次及操作权限。凭据轮换后需建立新连接；即使主体相同，新凭据也不能接管或查询原会话。

`Active` 表示逻辑连接，不代表传输在线、人在操作或 Computer 就绪。`Expired` 按数据库时间推导。`Closed` 和 `Revoked` 为持久化终态，有效能力列表为空。关闭递增连接 revision 和 revocation_revision；重复关闭不增加事件或 revision。撤销 Computer 的 `connect` grant 会在同一事务中把该主体已有 Active 连接标记为 Revoked；重新授权允许新连接，不复活原连接。凭据撤销、到期或主体禁用在每个端点重新鉴权时拒绝。

## 18.2 API 与权限交集

全部端点要求有效服务凭据及 `runtime.connect` scope。仅原凭据可查询或修改自身会话。OIDC/浏览器认证尚未实现，携带浏览器 `Origin` 的请求继续拒绝。会话 ID 是引用，不是 bearer token。

| 端点 | 契约 |
| --- | --- |
| `POST /v1alpha1/computers/{id}/connection-sessions` | 要求确切 Computer `connect` grant 和幂等键；返回 201 及当前会话元数据 |
| `GET /v1alpha1/connection-sessions/{id}` | 返回自身会话的当前状态与有效能力交集 |
| `POST /v1alpha1/connection-sessions/{id}/heartbeat` | 要求 Active 会话、预期连接 revision 及幂等键；返回 200 |
| `DELETE /v1alpha1/connection-sessions/{id}` | 幂等关闭自身会话；无需请求体或幂等键；返回 200 |

最小连接请求：

```json
{
  "requested_capabilities": ["connect", "read", "observe", "modify"],
  "lifetime_seconds": 900
}
```

能力列表有界、不允许重复，必须包含 `connect`；幂等摘要规范化列表顺序。请求体不接受组织、主体、凭据、generation、AgentSpec 或 caller 引用。AgentSpec/caller 扩展仍待实现；人和 Agent 连接均不依赖它们。

每个响应重新计算所请求能力、原凭据当前 scope 与当前 **Computer** 运行 grant 的交集。其它请求权限未获准时，可返回不包含它们的交集。只有交集中存在 `activate` 才返回 `max_runtime_seconds`，值取当前 grant 上限。响应不授予 Workspace、App 或私人 profile 权限，每个操作仍须独立校验全部关联资源。有权限也不代表后端操作已实现，应另查询服务能力端点。

```json
{
  "expected_revision": 1,
  "activity": "active",
  "visibility": "visible"
}
```

心跳中的 activity（`idle`/`active`）与 visibility（`hidden`/`visible`）明确属于客户端自报，不是已验证输入、画面确认、计费证据或自动停止依据。Viewer 帧及基于活动的停止策略另行实现。

## 18.3 事务、重试与上限

创建、心跳和关闭持有组织锁，元数据、事件与 Outbox 同事务提交。凭据/主体共享锁把鉴权与撤销串行化；写入后再次核验到期时间。写入失败或期间到期会回滚。心跳采用连接 revision CAS，与 Computer 控制/定义 revision 分开。身份绑定、请求能力及期限不可变；正常 store API 不能重新打开或删除终态历史。

创建和心跳重试保留原会话，返回其**当前**视图，不返回缓存的 Active 权限快照、不延长期限、不重复记录活动。同一幂等键改换请求返回 409；新心跳遇到非活动会话返回 410；跨凭据、主体或组织访问返回 404。凭据失效返回 401，缺少 scope 返回 403。写入结果未知时保留原键和输入重试；DELETE 自身幂等。

当前开发上限为每组织 256 个、每主体 32 个、每 Computer 64 个有效连接。计数要求 Active、未到期、原凭据仍有效且主体启用。准入串行化；关闭连接仅释放连接名额，不释放计算/存储预留。历史会话继续保留；保留期/GC 和可配置平台预算仍待实现。

## 18.4 验证与后续工作

新增 10 项 PostgreSQL 场景覆盖持久化/重试、WAL 重启、人/Agent 归属、凭据/组织隔离、权限变化、终态撤销、心跳竞争、到期、不可变字段、回滚、准入上限与迁移校验和。新增 2 项 HTTP 场景验证生命周期和身份字段拒绝；已有独立 TCP 进程测试也执行连接、心跳和关闭。数据库测试使用开启耐久配置的真实 PostgreSQL。

这是连接拥有者写租约/控制租约的前置权威元数据。租约、真实排空/fencing、受限连接 token、OIDC、浏览器传输、ViewerSession、基于活动的停止及完整 Computer 执行仍待实现。T01–T43 运行验收继续保持 `not_run`。关联文档：[15 运行授权](15-runtime-authorization.md)、[16 启动准入](16-start-admission.md)、[17 Candidate 准备](17-candidate-preparation-worker.md)。
