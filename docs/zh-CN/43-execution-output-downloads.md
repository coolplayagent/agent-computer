# 43. 认证后的执行输出下载

提交执行的凭据现在可以通过控制服务下载已保留的 stdout、stderr。网关从 S3 读取固定输出对象，每次验证长度和 SHA-256，完整验证后才返回字节，支持二进制和空输出。已有发布回执不代表对象目前仍可读取。

## 配置与调用

使用私有对象存储配置启动服务：

```sh
agent-computer-server serve --database-url-file /private/control-url --listen 127.0.0.1:8080 --output-config /private/output-store.json
```

`output-store.json` 采用 [36](36-durable-execution-outputs.md) 中的 `outputs` 配置：endpoint、region、bucket、私有凭据文件、可选 CA，以及隔离测试所需的显式 HTTP 开关。需要对原输出对象的认证 GET 权限，不需要 Kubernetes 或节点配置，也不读取本地发布 spool。可同时通过 `--file-config` 启用 Candidate 文件网关。端点必须匹配固定发布记录的 store digest；更改端点不会自动迁移对象。

| 请求 | 结果 |
| --- | --- |
| `GET /v1alpha1/executions/{id}/output` | 原有发布摘要；尚未捕获时为 null |
| `GET /v1alpha1/executions/{id}/output/stdout` | 已保留的标准输出字节 |
| `GET /v1alpha1/executions/{id}/output/stderr` | 已保留的标准错误字节 |

下载设置准确的 `Content-Length`，使用 `application/octet-stream`，附件名固定为 `stdout.bin` 或 `stderr.bin`，并设置 `Cache-Control: no-store` 与 `X-Content-Type-Options: nosniff`。`X-Output-Sha256`、`X-Output-Manifest-Digest` 关联固定发布；`X-Output-Observed-Bytes`、`X-Output-Truncated`、`X-Output-Eof` 保留采集边界。截断输出只返回已保留的前缀。响应不包含 bucket、对象 key、签名 URL、supervisor 报告或诊断。

每个流受原命令输出上限约束，最多 1 MiB。每个网关共有四个下载名额，克隆实例共享；名额覆盖对象读取、最终鉴权和响应体，直到 HTTP 传输层消费或丢弃响应体。超额请求直接返回 `503 output_io_busy`，不另建等待队列。沿用十秒请求期限；取消请求会丢弃异步对象读取，不遗留独立文件任务。不支持 Range、查询参数、偏移或实时尾随，相关输入会被拒绝。

## 当前授权与历史输出

下载需要原提交凭据仍有效，具备 `runtime.connect`、`runtime.read`，并保有 Computer 的 `connect`/`read` 和原 Workspace 的 `read` 授权。同一主体的新凭据不能接管旧执行；跨组织与不可访问的执行统一返回 404。

原连接可已关闭或过期，写租约可已释放，也可已进入后续 Candidate 或 writer epoch。读取历史输出不需要当前修改权限。系统通过固定 execution、epoch 找到原 Workspace，在 S3 I/O 前后分别检查当前授权，网络等待期间不持有数据库锁。下载期间撤销凭据、禁用主体、凭据过期或撤销资源读取授权都会阻止披露；对象 GET 失败时也执行最终鉴权。

尚未完成发布、对象缺失或校验失败返回 `503 execution_output_unavailable`，不返回部分字节。未配置网关返回 `503 outputs_unavailable`。仅配置成功时能力发现返回 `execution.output_downloads=bounded-verified-streams`。原有提交、摘要查询和取消契约保留。下载不会改变 execution 状态、启动授权、完成记录或写租约；Unknown 的观察输出可读，不会因此变成 Succeeded。

## 验证与范围

默认测试覆盖二进制和空流、WAL 恢复、待发布输出、跨凭据/组织拒绝、对象损坏、七种 GET 期间授权失效、历史读取、HTTP 配置和参数拒绝，以及响应体持有下载名额。真实执行夹具为十六个 gVisor 场景中的八份已发布输出启动新服务进程，分别下载两个流，并与原始报告逐字节、逐 hash 比较，包含命令失败、超时、截断以及存储/数据库失败后的输出恢复。

[绑定源码的组件记录](../evidence/execution-output-downloads-2026-10-10.json) · [验证日志](../evidence/execution-output-downloads-2026-10-10.log)

本次交付是有界输出读取能力。自动执行派发、浏览器/ComputerView、跨节点 fencing 和完整产品验收仍待完成。公开 `execution` 仍为 unsupported，Computer `ready=false`，T01–T43 保持 `not_run`。
