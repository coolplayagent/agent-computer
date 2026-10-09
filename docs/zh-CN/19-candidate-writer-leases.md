# 19. Candidate 持久化写入租约

## 19.1 所有权与授权

迁移 10 为每个已准备的启动请求增加一个修改租约头、不可变所有权 epoch、派发日志与排空证明。租约绑定当前 Computer generation、Candidate、原 ConnectionSession 及已验证准备收据的摘要，不分配新 Candidate、不复制文件，也不设置 Computer Ready。能力查询以 `candidate.writer_leases: control-plane` 表示这一阶段。

获取租约及每次续租/派发，都要求使用创建 Active 连接的原始凭据。连接必须请求且当前具备 Computer 的 `connect`、`read`、`modify`，并与凭据的 `runtime.connect`、`runtime.read`、`runtime.modify` scope 取交集；调用者还须独立拥有 Workspace 的 `read`、`modify` grant。创建声明、Computer `manage` 或同组织身份均不隐含这些权限。系统重新检查 Prepared 状态、generation、Candidate 身份及固定目录依赖的可用性。协作者使用自己的权限和凭据，不能沿用原启动者的授权。

租约默认且最长 30 秒，建议每 10 秒续租。请求时长必须为 1–30 秒；数据库时钟决定期限，且不能超过连接的固定截止时间。续租不缩短已有期限，也不延长连接。租约头的 revision 随写入递增；所有权 epoch 只在 Released 后重新获取时递增。即使已过期，第二个所有者也不能直接接管 Held 或 Draining 租约。

## 19.2 HTTP 契约

所有接口要求原连接凭据和 `runtime.connect`；获取及续租另须 `runtime.read`/`runtime.modify` 与上述独立 grant。浏览器认证实现前仍拒绝带 `Origin` 的请求。所有 POST 必须提供 `Idempotency-Key` 和严格 JSON；拒绝调用者填写 owner/principal 身份或自行声明进程已停止。

| 接口 | 结果 |
| --- | --- |
| `POST /v1alpha1/computers/{id}/leases` | 201，返回当前 modify 租约元数据 |
| `GET /v1alpha1/leases/{id}` | 200，返回自己当前 epoch 的视图 |
| `POST /v1alpha1/leases/{id}/renew` | 核对准确 owner/generation/epoch/revision 后返回 200 |
| `POST /v1alpha1/leases/{id}/release` | 有已确认排空证明时返回 200 Released；派发未确认则返回 202 Draining |

获取请求使用当前启动与连接响应中的 ID：

```json
{
  "scope": "modify",
  "connection_session_id": "connection-example",
  "candidate_id": "candidate-example",
  "generation": 1,
  "duration_seconds": 30
}
```

续租包裹准确的租约命令；释放直接使用内层 `lease` 对象：

```json
{
  "lease": {
    "connection_session_id": "connection-example",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "duration_seconds": 30
}
```

响应包含 ID、generation、epoch、revision、状态、截止/检查时间、`dispatch_recorded` 及可空的 `release_proof`，不暴露文件系统路径或存储凭据。ID 和响应 JSON 本身不是可重复使用的 IO 凭证。准确重试返回当前视图，不重复写入或延长有效期；进入后续 epoch 后重放旧 key 会失败。即使是同一主体，替换凭据也不能读取或接管旧所有者。冲突返回 409，失效连接返回 410，不可访问的所有者/资源返回 404，认证/scope 失败返回 401/403。

## 19.3 排空与恢复

过期或权限失效会使当前查询导出 Draining。关闭连接或撤销相关 Computer/Workspace grant 时，还会在同一事务将 Held 租约持久改为 Draining，并在对应事件记录数量；重新授权不能复活该 epoch。撤销凭据或禁用主体后，请求认证失败，但可信对账仍能降低权限。连接关闭或 grant 撤销后，所有者仍可使用有效原凭据发起释放，完成安全交接。

可信 Rust 边界 `begin_candidate_writer_dispatch` 先提交派发 ID 和规范化操作摘要，再返回绑定准备收据、不可 Clone 的许可。当前**每个所有权 epoch 只准入一次派发**。重复调用、丢失响应或 WAL 重启均不会重新发放许可。目前没有 HTTP 派发接口；有界文件 worker 已消费该许可并执行保守本地期限，通用进程执行器仍需监督及实际排空/隔离证据。

释放首先禁止后续准入。没有已提交派发的 epoch 可生成不可变 `no_dispatch` 证明并进入 Released；迁移 11 另允许封闭文件执行路径产生的 `bounded_file_drained` 证明。派发与证明插入锁定同一个租约头；SQL 守卫拒绝旧 epoch、派发后伪造零派发证明、历史改写及没有证明的释放。这份证明描述派发日志，不表示进程停止。记录过派发且没有封闭文件执行路径的排空证明时，释放、过期、连接关闭及对账均保留 Draining；后续能力见 [20 文件保存](20-bounded-file-saves.md)。调用者标志、超时或数据库租约本身都不足以允许接管。计算/存储预留继续保留。

运维可通过私有数据库 URL 文件执行一次对账：

```sh
agent-computer-server writer-lease-reconcile \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --lease-id lease-example
```

该命令不修改有效租约；对失效/过期所有权，仅在有零派发或已封闭文件排空证明时释放，否则返回 Draining。它不是 supervisor、定时调度器或物理 fence。修改持有组织锁，原子提交租约历史、幂等收据、事件与 Outbox；末尾授权/到期检查失败则完整回滚。

## 19.4 验证与下一边界

新增 12 项 PostgreSQL 场景覆盖 WAL 恢复、连接竞争、旧命令、期限、重放、单调 epoch、独立 grant、协作者凭据、撤权、派发/证明不可变、固定目录依赖变化、Outbox 回滚、凭据迟到过期及迁移校验和。新增 2 项 HTTP 场景覆盖生命周期、200/202 释放差异、严格请求结构与凭据隔离。已有独立 TCP 进程测试新增获取租约、关闭连接及运维对账命令。

测试使用真实 PostgreSQL 和模拟准备收据，证明控制面授权，不证明物理 IO 停止。Cargo/Bazel 默认测试现为 224 项，后续有界文件路径见 [20 文件保存](20-bounded-file-saves.md)。通用受监督进程写入、watchdog、物理排空/fencing、Workspace Pod 挂载、GUI 控制租约、Artifact 发布及完整 Computer 执行仍待实现；T01–T43 运行验收继续为 `not_run`。参见 [17 Candidate 准备](17-candidate-preparation-worker.md)、[18 连接](18-connection-sessions.md)与 [04 完整计划](04-implementation-plan.md)。
