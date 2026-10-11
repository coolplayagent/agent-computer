# 59. App 与 Browser 鉴权 CLI

CLI 现已接入 [App 准入](54-app-admission.md)与 [Browser 控制](58-browser-control.md) HTTP 接口。使用 `AGENT_COMPUTER_ENDPOINT`、`AGENT_COMPUTER_TOKEN_FILE`，或传入 `--endpoint`、`--token-file`。`doctor` 读取部署能力；本地 `client.apps`、`client.browser` 表示 HTTP 客户端支持，完整 Computer、Browser 运行时验收仍未完成。

| 命令 | `/v1alpha1` 下的 HTTP 请求 |
| --- | --- |
| `app start <computer-id>` | POST `/computers/{id}/apps` |
| `app show <app-instance-id>` | GET `/app-instances/{id}` |
| `app cancel <app-instance-id>` | POST `/app-instances/{id}/cancel` |
| `browser control show\|acquire <app-instance-id>` | GET/POST `/app-instances/{id}/browser/control` |
| `browser control release <app-instance-id>` | POST `/app-instances/{id}/browser/control/release` |
| `browser pages\|observe\|act <app-instance-id>` | POST `/app-instances/{id}/browser/commands` |
| `browser status <request-id>` | GET `/browser-commands/{id}` |
| `browser screenshot <request-id> --output <new-file>` | 同一鉴权 GET，再导出本地 PNG |

## 显式请求

POST 命令使用 `--request <file|-> --idempotency-key <key>`。例外是 `browser control release`：只传 `--request`，服务端比较原会话和控制 epoch；CLI 拒绝无效的幂等键选项。App cancel 仅取消派发前的 Queued/Blocked 实例，Running 会返回冲突；通过 CLI 停止运行中 App 仍待实现。control release 请求控制排空，HTTP 成功并不证明物理进程终止或控制交接完成。

启动请求示例，需替换为实际 ID 和版本：

```json
{"connection_session_id":"session_example","computer_generation":1,"app_id":"app_example","expected_app_revision":1,"runtime_seconds":30}
```

App 进入 Running 后显式取得控制：

```json
{"connection_session_id":"session_example","app_generation":1,"expected_epoch":0,"lifetime_ms":25000}
```

命令携带原会话、App 代次及取得的控制 epoch。CLI 要求 `command.kind` 与命令动词一致；服务端继续验证完整类型和当前授权。`browser observe` 示例：

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"observe","page_id":"page_example"}}
```

`browser act` 使用 [Driver 契约](53-browser-driver.md)中的动作；仅创建页面不依赖当前观察：

```json
{"connection_session_id":"session_example","app_generation":1,"control_epoch":1,"command":{"kind":"act","action":{"kind":"create_page"}}}
```

释放请求：

```json
{"connection_session_id":"session_example","control_epoch":1}
```

每次调用只发一个请求，不自动取得控制、轮询、重试、跟随重定向或重放动作。退出码 0 表示请求成功，仍须检查 Queued、Draining、Rejected、Unknown 等状态。写请求响应丢失时先查权威状态；显式幂等重试保留相同输入和原键。释放重试保留原会话与 epoch，不通过取得新控制重放不确定操作。读命令拒绝写请求选项。

## 截图导出

`browser status` 保留服务端 JSON。`browser screenshot` 要求已完成且仍获授权的观察，校验请求 ID、App 实例、控制 epoch，并要求结果截止时间位于本地时钟的未来 30 秒内。客户端与服务端需同步时钟。排队、过期或非观察结果不生成文件。

导出器限制 JSON 为 4 MiB、PNG 为 2 MiB，验证 PNG 签名、IHDR 尺寸、viewport、单位缩放和 SHA-256；不解压或认证页面控制的像素与 DOM。文件通过目标目录中的私有临时文件发布，已有路径不会被覆盖。JSON 保留观察身份、坐标及原有效期，用 `screenshot_file` 元数据替换内联 base64。保存文件不会延长后续输入权限，每个动作仍由服务端重新授权。

## 验证与后续范围

CLI 协议测试覆盖精确路由、原请求与键保持、Queued/Unknown/Draining、联网前拒绝、无键释放响应丢失、损坏或过期及错配截图、私有文件权限和覆盖保护。真实 CLI + HTTP + PostgreSQL 测试覆盖 App 幂等与冲突、显式控制、命令排队、释放/取消及撤权；其中 Running 前提使用模拟证据，没有启动物理 worker。实际浏览器执行证据仍见 [58](58-browser-control.md)。

原控制器仍限于 arm 后最长 30 秒窗口、拒绝全部出口网络及易失 profile。App 列表、受控出口、profile 检查点、ComputerView/WSS 输入仍待完成，本轮不宣称完整 Browser 能力已通过验收。

最终源码绑定验证见[证据记录](../evidence/browser-cli-2026-10-11.json)：20 个 Bazel 目标、535 个 Rust 与 19 个 TypeScript 测试通过，Clippy 和格式检查通过。实际运行证据的边界如上。
