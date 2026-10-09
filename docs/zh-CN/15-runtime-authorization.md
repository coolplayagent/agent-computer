# 15. 资源运行授权

## 15.1 已交付范围

迁移 6 新增独立于声明创建、管理和引用权限的资源运行 grant。运维可对指定 Computer、Workspace、App 或浏览器 profile 授权与撤权，凭据还须具有对应 API scope。创建声明或处于同一组织不会自动获得运行权限；迁移不扩展既有凭据的 scope。

Store 提供供后续生命周期准入复用的事务内授权函数、独立当前权限检查及有效权限视图；服务提供视图查询和受信任的本地授权命令。这些接口不分配 generation、不准备 Candidate、不启动 Pod、不预留修改租约，也不证明进程停止。后续 [16 启动准入](16-start-admission.md) 已实现 generation 分配与容量预留。不可变 Artifact 输入选择、Candidate 收据消费及物理 fencing 仍待实现。

## 15.2 权限与凭据约束

运行 scope 为 `runtime.connect`、`runtime.read`、`runtime.observe`、`runtime.app.use`、`runtime.activate`、`runtime.execute`、`runtime.modify`、`runtime.control`、`runtime.publish`、`runtime.manage`、`runtime.delete`，与 `definitions.validate` 和 `definitions.manage` 分离。签发接受这 13 种 scope 的非空子集，名称精确匹配且区分大小写。

| 目标类型 | 支持的 grant 权限 |
| --- | --- |
| `computer` | `connect`、`read`、`observe`、`app.use`、`activate`、`execute`、`modify`、`control`、`publish`、`manage`、`delete` |
| `workspace` | `read`、`modify`、`publish`、`manage`、`delete` |
| `app` | `read`、`observe`、`app.use`、`activate`、`control`、`manage`、`delete` |
| `browser_profile` | `read`、`app.use`、`manage`、`delete` |

权限之间不隐含授权：`manage` 不授予读取、控制或执行，GUI `control` 不授予任意 shell 或文件修改。没有通配符、按名称匹配、关联资源继承或组织级默认权限；涉及多个资源的操作须逐个检查所需目标/动作，每批检查接受 1–32 项不同要求。Computer grant 不披露 profile，profile 使用与内容读取分别授权。即使旧 grant 行还存在，被禁用的目录 profile 也不可用。

`activate` grant 必须具有 1–86,400 秒的 `max_runtime_seconds`；授权检查须提供不超过该上限的正整数运行时长，其他 grant 不携带时长。这是单次激活授权上限，不是 CPU/内存总预算、计量预留、实际运行 watchdog 或当前可启动的保证；后续运行时仍须分别实施这些条件。

## 15.3 事务与撤权契约

授权从凭据派生组织和主体，获取组织事件流锁，锁定并重新检查凭据/主体行，验证每个资源与 grant，并在返回前再次检查凭据。准入代码必须在持久记录授权效果的同一事务中调用事务内函数，不能把返回布尔值或有效权限响应复用为之后写入、后端派发的许可。

运行 grant 变更使用同一事件流锁；凭据撤销、主体禁用通过共享行锁与授权串行化。先取得相关锁的撤权会阻止等待中的检查成功。已经准入的请求仍可能结束，已下载数据也无法收回；已有执行取消、活动连接关闭及停止确认需要后续运行 worker。

grant 变更、顺序事件与 Outbox 同事务提交；重复相同 grant、上限或撤权不增加事件。上限变更发出 `runtime.permission_changed`，移除发出 `access.revoked`，事件明确记录 `process_termination_confirmed: false`。Outbox 失败会回滚授权变更及事件。这些管理命令要求受信任数据库访问，不提供可自我提权的 HTTP 写接口。

## 15.4 运维命令

先通过 [plan/apply](10-plans-and-apply.md) 发布资源，或注册浏览器 profile 目录引用；使用其稳定资源 ID，不带声明引用语法的 `id:` 前缀。主体必须已注册且启用，凭据签发会注册该身份。grant 元数据不包含 bearer token 或 profile 内容。

```bash
agent-computer-server credential-issue \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example --kind human \
  --scopes runtime.read,runtime.observe,runtime.activate \
  --ttl-seconds 3600 --output /private/control/runtime-token

agent-computer-server runtime-grant \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission read

agent-computer-server runtime-grant \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission activate --max-runtime-seconds 3600

agent-computer-server runtime-revoke \
  --database-url-file /private/control/database-url \
  --organization org_example --principal person_example \
  --kind computer --resource-id res_actual_computer \
  --permission activate
```

撤权不携带时长；收紧上限使用新的 `runtime-grant`。被移除或禁用的 profile 仍可撤销其遗留 grant。私有凭据与数据库文件规则沿用 [09 控制服务](09-control-service.md)。

`GET /v1alpha1/runtime-access/{kind}/{id}` 同时要求 `runtime.read` 及该精确目标的 read grant。结果只列出当前主体 grant 与所用凭据 scope 的交集，只有凭据也允许 activate 时才显示其上限。响应示例：

```json
{
  "kind": "computer",
  "resource_id": "res_actual_computer",
  "permissions": ["activate", "read"],
  "max_runtime_seconds": 3600,
  "checked_at_ms": 1791550000000
}
```

组织和主体不能通过路径、查询或正文选择。无效凭据返回 401，缺少 `runtime.read` 或带浏览器 Origin 返回 403，目标不存在、不可访问或已禁用统一返回 404；响应禁止缓存。此开发接口使用服务凭据，浏览器登录与 OIDC 待实现。Schema 已加入 [OpenAPI](../../schemas/openapi-v1alpha1.json)，能力将 `auth.runtime_grants` 标为 `control-plane`，`computer` 仍为 unsupported。

## 15.5 验证与剩余工作

10 项 PostgreSQL 场景覆盖 scope/grant 分离、精确资源/组织/主体匹配、私有 profile、激活上限验证与收紧、多资源检查、非法或重复请求、事件/Outbox 原子回滚、无变更重试、主体禁用及 grant/凭据撤销竞争，以及由上一版约束/迁移历史升级时保留既有凭据。新增 1 项 HTTP 场景覆盖 scope 交集、隐藏目标、Origin 拒绝、伪造主体查询和已撤销凭据；既有独立 TCP 进程场景还实际执行 `runtime-grant`、权限查询与 `runtime-revoke`。

执行[带数据库的 Cargo/Bazel 测试](08-persistence.md)。这些检查证明当前授权元数据行为。运行准入仍须在允许写入前绑定当前权限、generation、不可变输入、存储证据、租约与已确认 fencing；这些测试不会将 T01–T43 运行验收从 `not_run` 改为通过。
