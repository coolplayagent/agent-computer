# 52. 认证执行命令行

`agent-computer` 现在可以从终端或外部 Agent 调用已有的有界运行接口：建立逻辑连接、领取 Candidate 修改租约、提交结构化命令、查询或取消执行、下载保留的输出。鉴权、revision、幂等、派发和完成状态仍由服务端管理。提交成功返回 Queued，不代表进程执行成功。

## 配置与连接

运维方先配置控制服务和 worker、发布 Computer/Sandbox 定义，并为调用者签发有作用域的服务凭据和资源授权。客户端不读取控制数据库或 Kubernetes 凭据，也不创建身份、启动 worker 或自行授予权限。

```sh
export AGENT_COMPUTER_ENDPOINT=https://computer.example.org
export AGENT_COMPUTER_TOKEN_FILE=/private/computer-token
agent-computer doctor --json
agent-computer computer show cmp_example --json
agent-computer computer start cmp_example --request start.json --idempotency-key start_001 --json
```

`start.json` 使用已有启动准入请求：

```json
{"expected_revision":1,"expected_spec_revision":1,"max_runtime_seconds":300}
```

保留回执的 request ID、Candidate ID 和 generation，通过 `computer show` 查询准备 worker 是否已报告 Prepared。此状态允许使用有界 Candidate 接口；整台 Computer/App 的就绪检查尚未完成，因此 `ready=false` 仍然准确。`doctor` 读取服务能力声明，不是部署认证或 worker 健康检查。

创建 `connect.json`，显式建立连接：

```json
{"requested_capabilities":["connect","read","modify"],"lifetime_seconds":900}
```

```sh
agent-computer connect cmp_example --request connect.json --idempotency-key connect_001 --json
```

将连接返回的 session ID 和启动回执填入 `lease.json`：

```json
{"scope":"modify","connection_session_id":"session_example","candidate_id":"candidate_example","generation":1,"duration_seconds":30}
```

```sh
agent-computer lease acquire cmp_example --request lease.json --idempotency-key lease_001 --json
```

## 提交与观察

将租约返回的 ID、generation、epoch、revision 和已配置的 Sandbox ID 填入 `execution.json`。领取短租约前先准备好模板，并在有效期内提交；续租需要显式调用。示例 ID 均为占位符；CLI 不自行发现或替换授权字段。`argv` 保持结构化 JSON，不经本地 shell 求值。

```json
{"lease_id":"writer_example","lease":{"connection_session_id":"session_example","generation":1,"epoch":1,"expected_revision":1},"sandbox_id":"sandbox_example","command":{"argv":["/bin/echo","hello computer"],"cwd":"","timeout_seconds":10,"term_grace_ms":500,"output_limit_bytes":4096}}
```

```sh
agent-computer exec cmp_example --request execution.json --idempotency-key execution_001 --json
agent-computer status exec_example --json
agent-computer logs exec_example --json
agent-computer logs exec_example --stream stdout --output stdout.bin --json
agent-computer logs exec_example --stream stderr --output stderr.bin --json
agent-computer disconnect session_example --json
```

不带 stream 的 `logs` 返回输出发布元数据，尚未捕获时返回 null。下载需要服务端输出网关和仍有权限的原提交凭据。CLI 校验长度、SHA-256 和采集元数据后才发布私有新文件；已有文件或符号链接均不覆盖，输出字节不会直接写到终端。JSON 回执保留观察字节数、保留字节数、截断和 EOF 信息，不能把不完整或截断的观察当作执行成功。

`disconnect` 只关闭逻辑连接。新执行默认 background lifetime；已提交的后台执行可在原授权和预算内继续。要随连接停止，请在执行请求中显式设置 `"lifetime":"connection"`。CLI 不自动续期连接、写租约或执行租约。

取消时，将当前执行 revision 填入 `cancel.json`，例如 `{"expected_revision":1}`，然后运行：

```sh
agent-computer cancel exec_example --request cancel.json --idempotency-key cancel_001 --json
```

必须检查回执状态：已请求取消不等于进程已终止。Unknown 只有在权威证据确认后才能解决。

## 命令与请求契约

| 命令 | 已有接口 |
| --- | --- |
| `doctor` | GET `/v1alpha1/capabilities` |
| `computer show/start/cancel-start/stop/checkpoint-stop ID` | `/computers/{id}` 下的 GET `runtime` 或 POST `start`、`start/cancel`、`stop`、`checkpoint-stop` |
| `connect ID`、`connection show/heartbeat ID`、`disconnect ID` | 连接创建、查询、心跳、DELETE |
| `lease acquire/show/renew/release ID` | Candidate 修改租约；acquire 使用 Computer ID |
| `exec ID`、`status ID`、`cancel ID` | 执行提交、查询、取消；提交使用 Computer ID |
| `logs ID`，可加 `--stream stdout|stderr --output FILE` | 发布元数据或校验后的输出下载 |

远程命令可通过 `--endpoint`、`--token-file` 覆盖环境变量。POST 必须提供 `--request FILE|-` 和显式 `--idempotency-key`，请求限一个不超过 64 KiB 的 JSON 对象。字段定义见 [OpenAPI](../../schemas/openapi-v1alpha1.json)、[连接](18-connection-sessions.md)、[写租约](19-candidate-writer-leases.md)、[执行](23-execution-admission.md)、[输出](43-execution-output-downloads.md)及[检查点停止](46-checkpoint-stop-worker.md)。经 `bazel run` 启动时，请求文件和凭据路径相对最初调用目录解析。

成功 JSON 写 stdout，错误写 stderr；`--json` 使用紧凑格式。退出码 0 表示 HTTP 操作被接受，1 表示 HTTP 拒绝，2 表示输入/配置错误或校验未完成。HTTP 错误保留服务端 code、details 和 request ID，并补充 `http_status`。传输错误不回显 URL、请求体或凭据。写请求没有收到可信响应，或收到 HTTP 408、5xx、重定向时，返回 `request_may_have_been_applied=true`：应查询权威状态，或使用原幂等键和完全相同输入显式重试，不能因响应丢失就换新键。

只接受 HTTPS origin 或字面量 loopback HTTP origin，拒绝用户名密码、路径前缀、查询参数和片段；loopback 绕过环境代理。不跟随重定向，不自动重试。连接超时 5 秒，HTTP 总超时 15 秒；JSON 响应上限 4 MiB，单路输出上限 1 MiB。本轮尚无自定义 CA、不安全 TLS、原始 API、自动轮询或实时输出 tail 选项。

## 验证范围

线路测试覆盖路由、原样 argv/幂等键、代理绕过、拒绝重定向、传输不确定性、输入/响应限制、二进制和空输出、截断、损坏、不完整响应及已有文件保护。真实 CLI 子进程经 TCP 访问实际 Axum 服务和 PostgreSQL，验证连接/租约准入、重试仅生成一条执行、冲突、后台断开、状态、取消和主体撤权。测试准备回执为合成数据，SQL 确认派发意图为零，因此只证明客户端与控制面行为，不新增 gVisor 或存储持久性证据。完整 Computer 生命周期、Browser/ComputerView、OIDC 和 T01–T43 产品验收仍未完成。
