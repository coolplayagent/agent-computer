# 10. 声明计划与原子 apply

## 10.1 已交付边界

控制 API 现可规划并发布 ComputerSet 的六类资源：Volume、Workspace、Sandbox、App、Agent、Computer。计划包含解析后的前后规格、稳定资源 ID、固定依赖 revision/digest 及排空要求。apply 在 PostgreSQL 一次事务中提交声明版本、变更资源的 SpecVersion、创建者授权、operation、协调意图、幂等回执和事件/Outbox。

服务将 `definitions.plan`、`definitions.apply` 标记为 `control-plane`。apply 创建 `Queued` operation 和 `Pending` 意图。[持久化协调接口](11-reconciliation-coordination.md)可推进这些状态，但尚无后端 worker 启动 Kubernetes 资源、文件系统、浏览器或托管 Agent。`requires_drain` 记录后续 worker 的要求，不证明任何进程已经停止。[运行验收矩阵](../../codespec/test/agent-computer.md)仍未执行。

## 10.2 Scope 与资源权限

按 [09 控制服务](09-control-service.md)迁移/启动服务，并签发包含 `definitions.manage` 的凭据。需要静态验证时另加 `definitions.validate`。身份与组织完全来自凭据。

可信本地运维为已有主体授予声明权限：

```bash
bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind declaration --name '*' --permission create

bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind agent --name '*' --permission create
```

这两条 grant 足以创建只包含 external Agent 的 ComputerSet。完整 ComputerSet 还需每种声明资源的创建权限，以及其目录依赖的引用权限。凭据 scope 本身不授予这些权限。

| 权限 | 含义 |
| --- | --- |
| `create` | 在本组织创建指定种类的声明/资源；name 必须为 `*` |
| `manage` | 规划/更新准确名称，或用 `*` 覆盖全部名称；同时允许引用该资源 |
| `reference` | 引用已有资源/目录记录，不允许修改 |

种类为 `declaration`、`volume`、`workspace`、`sandbox`、`app`、`agent`、`computer` 及下节目录种类。目录创建使用运维命令，不接受 `create` grant。首次 apply 成功后，创建者自动获得每个新声明/资源的准确名称 `manage` grant。名称保持保留，尚未实现删除和名称复用。使用参数相同的 `definition-revoke` 撤销 grant。运行时 observe/execute/control、组织成员关系与 OIDC 授权仍是后续工作。

读取计划、读取 operation 和重试都要检查原计划所需权限当前仍有效，包括原始 `create` grant。因此即便创建者后来已有 `manage`，撤销 create 仍可能使旧回执不可访问。引用已有 App/Sandbox 时，还检查全部间接 profile、Secret、网络与 Workspace 依赖权限，不能借可访问的父对象绕过私有依赖授权。

## 10.3 目录引用与版本

| 种类 | ComputerSet 字段 |
| --- | --- |
| `storage_class` | Volume 的 `storageClass` |
| `network_policy` | Sandbox 的 `networkPolicyRef` |
| `browser_profile` | Browser App 的 `profileRef` |
| `secret` | Agent 的 `secretRefs` |

```bash
bazel run //:agent-computer-server -- catalog-register \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --kind storage_class --name juicefs-workspace

bazel run //:agent-computer-server -- definition-grant \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal planner \
  --kind storage_class --name juicefs-workspace --permission reference
```

注册只产生引用元数据，不部署 JuiceFS、验证网络策略、存储秘密值或创建浏览器 profile。重复注册返回稳定 ID。`catalog-disable --database-url-file PATH --organization acme --resource-id ID` 禁用记录并递增 revision；重新注册不会重新启用它。后端配置与能力探测待实现。

声明内名称解析为稳定的 `res_…` ID。声明外已有资源必须使用 `id:<resource_id>`；目录引用可用名称或 `id:<resource_id>`。解析检查组织、种类和权限。规格保存解析后的引用及不可变依赖 revision/digest。App 和 Computer 必须选择相同 Sandbox 版本；Sandbox 可写挂载必须选择 Computer 主 Workspace 的同一版本。因此重新规划时依赖变更也会改变依赖它的规格摘要，旧版本继续保持原固定依赖。

## 10.4 HTTP 流程

四个端点都要求单个 bearer 请求头、`definitions.manage`，使用 JSON、拒绝浏览器 Origin，并返回 `Cache-Control: no-store`。准确响应 Schema 见 [OpenAPI 契约](../../schemas/openapi-v1alpha1.json)。

| 请求 | body / header | 结果 |
| --- | --- | --- |
| `POST /v1alpha1/plans` | ComputerSet；`Idempotency-Key` | 201 不可变计划 |
| `GET /v1alpha1/plans/{id}` | 无 body | 200 本主体拥有的计划 |
| `POST /v1alpha1/plans/{id}/apply` | `{"plan_digest":"sha256:…"}`；`Idempotency-Key` | 202 operation 及当前状态 |
| `GET /v1alpha1/operations/{id}` | 无 body | 200 本主体拥有的 operation |

最小首次计划 body：

```json
{
  "apiVersion": "agent-computer/v1alpha1",
  "kind": "ComputerSet",
  "metadata": {"name": "research"},
  "spec": {
    "agents": [{"name": "external", "mode": "external", "adapter": "tools-api"}]
  }
}
```

检查返回的 `resources`、`before`、`after`、`dependencies` 和 `requires_drain`，再以另一请求键将准确 `plan_digest` 提交到对应 apply 路径。计划不可变，首次 apply 有效期按数据库时间为 15 分钟。body 上限 1 MiB，序列化预览上限 8 MiB，资源上限 1024，间接依赖遍历最多 16384 个不同版本引用。规划会保存元数据与事件，但不发布资源或意图。

更新时，将根 `metadata.expectedRevision` 设为当前声明 revision，每个已有资源的 `expectedRevision` 设为当前资源 revision。即使全部资源规格未变，成功 apply 仍递增声明 revision；变更资源追加下一不可变版本，未变资源保留 revision/digest。新资源省略 `expectedRevision`。已有对象缺少版本条件返回 428，规划或首次 apply 时版本不符返回 412。计划也固定外部资源当前版本和目录版本，首次 apply 前发生变化必须重新规划。

请求键为 1–128 个 ASCII 字母/数字/下划线/连字符，按组织、主体和操作隔离。相同键变更输入返回 409，回执退役返回 410。重试计划返回相同不可变预览。重试已 apply 计划会返回同一 operation 的当前进度，换新键也不会重复发布；过期不撤销已经完成的准入。每次仍检查当前权限。超时/断连可能使提交结果未知：使用同键同输入重试，再按返回 ID 查询。

未 apply 的过期计划返回 410，摘要不符 409，不可访问计划 404，缺少顶层 grant 403，不存在/种类错误/未授权依赖 422。无效声明及未支持的数据迁移也返回 422。错误沿用包含 request ID 的公共结构。

## 10.5 原子性、安全与验证

组织序号行锁串行化 grant、目录、声明写入和事件发布。取得行锁后，plan/apply 再次检查凭据，并将凭据/主体共享行锁保持到提交；先取得准入锁的撤销或禁用操作会阻止写入。事务结束前再次检查凭据到期。这只覆盖控制数据库准入，后续 worker 仍需在运行派发时重新鉴权。

apply 检查每个选定 revision 后在单事务保存全部元数据。按依赖顺序为每个声明资源排入一条意图，包括未变资源。不声称存在跨 Kubernetes/S3 事务。省略资源保持不变，不删除数据。Volume 切换 storage class/缩小配额以及 Workspace 更换 Volume 均被拒绝，等待明确的数据迁移/删除契约。

新增十二项真实 PostgreSQL 场景覆盖七资源发布、稳定身份/不可变历史、依赖更新、并发重试/竞争计划、末尾写入回滚、主体隔离、引用权限、准确版本兼容、撤权锁竞争、过期计划及退役请求键。过期场景通过受控数据库 fixture 将不可变计划设为已过期，不实际等待 15 分钟。HTTP 测试覆盖 plan/apply 错误及重试，独立 TCP 服务进程覆盖运维 grant、目录管理、plan/apply 和凭据撤销。复现使用[带数据库的测试命令](08-persistence.md)。这些证据仅证明本地控制面行为；运行协调、分布式授权、物理 fencing 与生产部署仍待实现。
