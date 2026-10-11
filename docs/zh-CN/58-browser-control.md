# 58. Browser 控制、观察与动作队列

[持久启动控制器](57-browser-controller.md)现在串行消费鉴权 GUI 请求。迁移 33 保存控制代次、有限队列和不可变动作意图/回执；原 Browser attach 与节点 guard 仍是执行前提。该增量提供 HTTP 接口，ComputerView、WSS 输入流和公开 Browser 能力认证仍待交付。

## 权限和控制交接

身份从服务凭据推导，连接必须属于该凭据和主体，且绑定原 Computer。观察要求 Computer/App 的 read、observe，以及明确的 profile read；控制另需 Computer/App 的 control、app.use 和 profile app.use。连接必须请求对应能力。Workspace 成员身份、只读权限和运维查看记录都不能授予 GUI 控制。

每个 Browser 的全部页面共享一个控制代次。领取带 `expected_epoch`，首次为 0，下一次只能从已排空的 Idle 领取；并发最多成功一个。租期为 1–30000 毫秒，同时截断到原 App 与节点 arm 截止时间，本轮没有续租。授权撤销、连接关闭、租期届满和显式交还使旧控制进入 Draining；重新授权不能复活它。

原 worker 等当前请求结束后，通过同一私有通道发送排空屏障。驱动串行释放离散按键/鼠标输入、清理观察缓存并返回关联代次，Store 校验原 App、操作、claim 和活 guard 后才记 Idle。未知动作、屏障失败或确认丢失阻止交接。下一拥有者使用递增代次并重新观察，旧帧不能作为点击依据。本轮不包含按下保持、拖拽或 WSS 输入批次。

## HTTP 使用

接口均位于 `/v1alpha1`。使用服务 Bearer 凭据和私有连接，远程访问沿用 TLS 反向代理；Origin 请求仍被拒绝，不是浏览器前端登录通道。

| 接口 | 作用 |
| --- | --- |
| `GET /app-instances/{id}/browser/control` | 当前控制元数据；无控制记录时为 null |
| `POST /app-instances/{id}/browser/control` | 用幂等键领取控制 |
| `POST /app-instances/{id}/browser/control/release` | 请求原控制排空，202 不代表已交接 |
| `POST /app-instances/{id}/browser/commands` | 用幂等键提交 pages、observe 或 act，返回请求 ID |
| `GET /browser-commands/{id}` | 重新鉴权后查询原请求及可返回的结果 |

领取参数是 `connection_session_id/app_generation/expected_epoch/lifetime_ms`；交还是 `connection_session_id/control_epoch`。队列输入包含 `connection_session_id/app_generation/control_epoch/command`，不接受主体、组织或派发许可覆盖字段。例如：

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}
```

pages 返回页面列表；observe 指定 `page_id` 并返回关联的 PNG、摘要、DOM 引用和视口信息。act 使用[驱动动作契约](53-browser-driver.md)，除创建页面外均携带 page/observation 引用。队列最多保留 128 个待处理请求，实例生命周期最多接收 1024 个请求；请求体上限 64 KiB，单份结果上限 3 MiB，总结果预算 64 MiB。没有自动重试派发或接管旧 attach。

## 不确定性与结果保密

worker 在写私有通道前提交唯一 dispatch claim。动作另在实际输入前提交完整意图，并再次在线检查控制权限。`delivered` 仅表示驱动完成输入，不证明网站业务成功。完成记录丢失会保留 Unknown；同键重试只查询原请求，不能再次输入。若完成日志已提交但后续请求状态确认丢失，查询可从该不可变回执恢复结果，仍不重发动作或恢复控制权。

观察在发布和读取时重新鉴权，绑定原凭据/连接、App/Computer、操作与控制代次，并扣除处理耗时；过期或交接后的帧不可读取。动作回执可在 App 排空后凭当前读取权限查询。控制库保存敏感页面结果及动作正文，事件只含 ID、代次和状态，不含截图、DOM 或输入文字。本轮没有新增结果清理/保留期服务。

原运行窗口仍最长为 arm 后 30 秒。实例的 Running 是历史启动观察，`ready` 仍为 false。当前 Kubernetes 模板维持全拒绝网络策略，公共网站导航和受控出口尚未接入；本轮真实端到端验证使用空白页创建、截图、关闭和交接，完整 DOM 输入由独立 Chromium 驱动测试覆盖。profile 仍为私有易失状态，停止进程不代表持久检查点、槽位释放或完整 Computer 生命周期完成。

## 验证证据

[源码与运行记录](../evidence/browser-control-2026-10-11.json)和[测试输出](../evidence/browser-control-2026-10-11-tests.log)关联全仓检查、三个真实控制场景、十二个 Chromium 驱动场景和四个 worker 场景及其产物。记录单独标明真实运行后补充的拒绝观察查询修复及 PostgreSQL 回归，避免混淆验证快照。确认原进程与 Pod 不再存在后，已清理临时虚拟机、私有磁盘及凭据。
