# 54. App 持久化准入

控制服务现可为 Prepared Computer 的某一代预留 Chromium App、查询预留状态，并在派发前取消。迁移 31 持久化固定 App/Sandbox 版本、连接和凭据绑定、App 代次、截止时间与独占槽位，为[Browser Driver](53-browser-driver.md) 接入补齐持久化前置条件。该接口不启动浏览器，也不提供 GUI 控制：`app.admission` 为 `bounded-queued`，`browser` 仍为 `unsupported`，App 响应始终是 `ready: false`。

## 请求与授权

| 操作 | 接口 | 授权要求 |
| --- | --- | --- |
| 准入 | `POST /v1alpha1/computers/{id}/apps` | 绑定的活跃连接；Computer connect/read/app.use/activate；App read/app.use/activate；Profile app.use 及显式主体归属 |
| 查询 | `GET /v1alpha1/app-instances/{id}` | 原主体的当前 Computer/App read grant 和 runtime.read scope |
| 取消 | `POST /v1alpha1/app-instances/{id}/cancel` | 原主体的当前 Computer/App manage grant 和 runtime.manage scope |

准入还要求所有对应凭据 scope 和连接请求的 Computer 能力，权限不互相隐含。两个 POST 都要求幂等键，接受最多 4 KiB 的未压缩 JSON；未知身份、归属、ready、截止时间和 binding 字段均被拒绝。不接受带浏览器 Origin 的凭据调用。[OpenAPI](../../schemas/openapi-v1alpha1.json) 定义响应及错误。

```json
{
  "connection_session_id": "conn_example",
  "computer_generation": 1,
  "app_id": "app_definition_example",
  "expected_app_revision": 1,
  "runtime_seconds": 300
}
```

以上 ID 仅示意，应使用实际连接、Computer 代次及固定的 App 定义。准入核对 App 属于 Computer 的不可变启动快照、版本一致、驱动为 `chromium-playwright`、Sandbox 为 `gvisor`，并重新校验目录引用。当前 Computer/App activate grant、原 Computer 的运行预留和绑定连接共同限制 1–300 秒的请求区间。固定截止时间从准入开始计算，包含排队时间；查询、重试均不续期。这是预留上限，不是物理进程 watchdog。

注册 BrowserProfile 目录引用或授予 app.use 不会建立 profile 主体。可信管理员必须通过 `Store::register_browser_profile_owner`，将已启用的 profile 显式绑定到已启用主体。绑定不可变，接口不允许转移。调用者既必须是该主体，也必须拥有当前 Profile grant。迁移不从创建者权限或组织成员身份推断归属；准入不产生 Cookie、profile 内容、checkpoint 或恢复结果。

可信管理员可通过现有运维命令注册主体，无需自行编写 Store 客户端：

```bash
agent-computer-server browser-profile-owner-register --database-url-file /private/database-url --organization acme --resource-id PROFILE_ID --principal alice
```

需使用私有数据库 URL 文件及已启用的主体/profile。同一归属重复注册是幂等的，更换主体会失败；该命令不授予 app.use，也不启动运行时。

## 预留与恢复

组织事务锁串行化准入、取消和资源 grant 修改。每个 Computer/App 只能有一个未取消实例；每个准备请求/Sandbox 只能占用一次；每个组织/Profile 同时只能被一个实例占用。共享 Sandbox 的不同 App 不能重复使用同一份 CPU/内存预留，现有执行准入也已排除 App Sandbox。Queued 和 Blocked 都占用每组织 64、每主体 8 的容量上限。App 代次在取消和 Computer 换代后继续递增，Store 与 SQL 变更保护禁止删除或改写历史。

状态为 `Queued → Blocked → Cancelled` 或 `Queued → Cancelled`。凭据撤销、主体禁用、连接关闭/撤销会在同一事务中写入 Blocked；移除必要 grant 或降低 activation 预算也会永久阻断预留，恢复权限不会使其复活。查询和原请求重试还会核对原身份、连接、grant、目录、Computer 代次与期限，过期或失去授权后转为 Blocked。Blocked 继续占位，直到显式取消；它不是进程已终止的证据，本增量尚无派发路径。

取消需提供 `app_generation` 和 `expected_revision`。同键同输入重试只返回原记录当前状态，不创建新实例。原主体的新凭据可按当前 grant 查询或取消，但不能继承旧连接、复活 Blocked 准入。任何 App 预留仍存在时，Computer 都不能停止或封存 Candidate；Store 前置检查和数据库保护共同强制此约束，过期记录也不例外。应先取消尚未派发的 App。

事件只记录标识、版本和 binding 摘要。HTTP 元数据不返回固定镜像/命令正文、服务凭据、本地 profile 路径或 profile 数据。数据库凭据及可信 Store 管理操作不进入未来 Browser Pod。

## 验证边界

PostgreSQL 与 HTTP 契约覆盖 WAL 重启、从迁移 30 升级、幂等、并发归属、不可变 Profile 主体、旧代次、独立 scope、凭据/grant/连接失效、固定过期、取消及 Computer 停止/封存阻断。另一个 HTTP 场景使用两个不同 App、两个不同 Profile 和同一个 Sandbox，验证预算槽位独占。夹具中的存储准备回执明确为合成证据，仅证明数据库和服务行为，不启动 Chromium，也不认证部署环境。

下一步仍须接入实际 App 启动 worker 与 Pod 身份、节点 fencing、持久 GUI 控制代次与动作 journal、BrowserDriver 可信桥接、profile checkpoint/restore 和 ComputerView。Queued 记录不等同于生产 `Authority` 或 `ActionJournal` 适配器。完整 D09 与 T04/T05/T10/T24 验收仍未完成。[本增量源码绑定记录](../evidence/app-admission-2026-10-11.json)保留这些限制。

[Browser worker 桥接](55-browser-worker-bridge.md)现提供私有进程通信入口，持久控制器和节点接入仍待完成。
