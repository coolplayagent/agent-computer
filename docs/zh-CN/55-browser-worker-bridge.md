# 55. Browser worker 桥接

内部 Browser Driver 现有独立、有限时长的 stdio worker 进程，通过私有管道向可信控制器请求授权和动作日志确认，使 Browser Pod 无需保存数据库、Kubernetes 或服务凭据。本次交付进程通信组件；[App 准入接口](54-app-admission.md)仍不派发，公开 Browser 能力仍为 unsupported。

## 入口和绑定

构建 `//packages/browser-driver:worker`，或编译固定版本的 TypeScript 包后执行 `dist/src/worker.js --stdio-binding-json '<binding JSON>'`。运维方传入持久化绑定，字段严格为 `scope`、`actor`、`viewport`、`runtime_ms`，结构见[驱动契约](../../packages/browser-driver/src/contracts.ts)。Scope 包括组织、Computer 代次、App 实例/代次及 profile 主体。Runtime 范围为 1,000–300,000 ms；未来控制器必须传入准入剩余时间，不能在排队后重新开始原时长。

镜像须提供匹配的 Playwright/Chromium，以及 worker 所有、0700 权限的本地私有目录 `/browser-state`。每次启动在其中创建独立 profile。本次不恢复或发布 checkpoint。协议不接受 profile 路径、可执行文件、镜像、Chromium 参数、脚本或凭据。库入口 `serveBridge` 可由可信宿主调用；可选 launch factory 用于受控组合和测试，不是协议字段。

## 消息与顺序

消息是严格 UTF-8 JSON 对象加 LF。缺字段、多余字段、非法操作或帧格式导致会话终止。版本为 1。控制器先发送 `hello`，字段为 `version`、`kind`、`binding_digest`。导出的 `bindingDigest` 对 `agent-computer/browser-bridge/v1`、NUL 和规范化绑定的排序键 JSON 计算 SHA-256。摘要只用于关联，不认证调用者。

Worker 返回 `kind: challenge`、version、随机 `session_id` 和相同摘要；后续消息都绑定此 session。请求字段为 version、`kind: request`、session_id、从 1 连续递增的整数 request_id、operation：

| method | operation 的附加字段 |
| --- | --- |
| launch | 无；只允许一次，先于其他操作 |
| pages | actor |
| observe | actor、page_id |
| act | actor、action_id、action |
| close | 无；终止会话并尝试关闭 |

一次只执行一个操作，禁止并发流水线。控制器可能先收到完整结果，worker 写回调随后才返回，因此结果写入期间允许暂存一个下一请求。Page、observation 和 action 仍经驱动校验。截图结果以 `screenshot_base64` 替代内部 Buffer，SHA-256 仍对解码后的 PNG 字节计算。

Worker 的 `callback` 含 version/session/request ID、递增 callback_id、method 和 payload。Method 为 authorize、begin、complete，对应 `AuthorizationRequest`、`ActionIntent`、`ActionReceipt`。控制器回复严格包含 version、`kind: reply`、session/request/callback ID、相同 method、ok、value。拒绝为 `ok: false, value: null`，不传上游错误文本。成功值分别为 Grant、日志 dispatch/replay 决策或完成持久化后的 null。缺失、重复、未经请求或错配的回复不能解除等待。

`result` 包含 version/session/request ID、kind、ok、value。失败仅返回驱动错误码或 bridge_failure。动作外层 ok 为 true 仍可能带 `recorded: false` 或 unknown receipt，必须继续检查。Close 返回 driver_state 和 `physical_termination: false`，即使驱动显示 closed。CLI 退出 0 仅表示 close 请求完成，其他终止退出 1，非法 CLI 参数结构退出 2。退出码和序列化结果均不证明节点物理 fencing。

## 可信控制器职责

管道只能由已准入控制器持有，协议 ID 不是身份凭据。控制器必须将每次回调的 scope、actor、permission、action ID、control epoch 和完整规范化 action 与自己正在处理的操作比较，并检查原凭据/连接、当前授权、App/profile ACL、代次及剩余预算。先持久提交唯一、不可变 intent，再允许 dispatch；先提交结果，再确认 complete。既有未解决 intent 只能进入 Unknown/对账，不能再次派发。

不能只信任驱动摘要：应保留完整规范化输入和服务端独立摘要，跨语言 JSON 数值编码可能不同。Complete 应答拒绝或丢失会返回 recorded=false、Unknown 并阻止新控制，不重复效果。重连、重启进程、更换请求或动作 ID 都不证明旧效果未发生；持久控制器须对账原 intent 和原物理实例。

后续 [Browser 运行时组件](56-browser-runtime.md)已提供 Rust 协议、独立 Kubernetes Pod/attach 和 Browser 节点保护；PostgreSQL 回调、GUI control epoch 及生产 App 派发/Ready 仍待接入。测试回调不构成生产授权。

## 边界与验证

输入每帧 64 KiB、会话累计 16 MiB、最多 16,384 帧；输出每帧 3 MiB、累计 64 MiB，最多 1,024 个操作。握手期限 15 秒，固定 runtime 从 hello 起计算并包含启动延迟，驱动另行消耗授权预算。写入期限 10 秒。EOF、传输/协议故障和超时拒绝等待中的回调并启动关闭；清理最多等待 20 秒，不确定关闭仍需独立节点 fencing，管道不可复用。

`bazel test //packages/browser-driver:bridge_test` 不依赖 Chromium，覆盖分帧、关联、并发、配额、断线、回调取消与错误脱敏。Manual 目标 `//packages/browser-driver:bridge_browser_test` 需要原浏览器测试同样的 sandbox-enabled Chromium 及私有 `/browser-state`；它通过实际 OS 管道运行 worker，验证截图、日志顺序、重试、结果确认丢失、错范围授权和断线。控制器回调为测试 fixture，不能等同部署后的授权/日志服务。[源码绑定验证记录](../evidence/browser-worker-bridge-2026-10-11.json)保留实际结果和环境限制。
