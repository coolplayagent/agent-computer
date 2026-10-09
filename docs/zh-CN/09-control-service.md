# 09. 控制服务与服务凭据

## 09.1 启动开发服务

`agent-computer-server` 是由 Bazel 构建的 Rust/Tokio/Axum HTTP 入口。目前提供探活、Schema 就绪检查、版本/能力、OpenAPI，带认证的 ComputerSet 静态验证及[授权 plan/apply](10-plans-and-apply.md)。资源发布只排入协调意图，启动 Computer 仍需后续运行 worker。

准备 PostgreSQL 数据库，将连接 URL 放入可信目录下、权限为 `0600` 或 `0400` 的普通文件。拒绝符号链接及组/其他用户可访问的文件，URL 最多 8 KiB。通过文件提供 URL，不将秘密放在命令行参数中。启动前显式迁移：

```bash
bazel run //:agent-computer-server -- migrate \
  --database-url-file /run/secrets/agent-computer/database-url
bazel run //:agent-computer-server -- serve \
  --database-url-file /run/secrets/agent-computer/database-url \
  --listen 127.0.0.1:8080
```

默认监听 `127.0.0.1:8080`。进程提供 HTTP，远程客户端须经过 HTTPS 反向代理和私有后端连接。TLS 终止、请求头/连接限制、生产数据库角色及部署认证仍待交付。服务收到 SIGINT/SIGTERM 后优雅退出；连接池上限 16，获取连接超时 5 秒。请求期间不会执行迁移，启动时也不静默升级。

## 09.2 凭据生命周期

凭据管理是使用可信数据库访问的本地运维命令。服务令牌不能通过 HTTP 签发凭据、代选请求主体或禁用其他主体。签发时创建或复用组织和 `human`/`agent` 类型固定的已启用主体；类型冲突或已禁用主体均拒绝。

```bash
bazel run //:agent-computer-server -- credential-issue \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal validator --kind agent \
  --scopes definitions.validate --ttl-seconds 3600 \
  --output /run/secrets/agent-computer/validator-token

bazel run //:agent-computer-server -- credential-revoke \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --credential CREDENTIAL_ID

bazel run //:agent-computer-server -- principal-disable \
  --database-url-file /run/secrets/agent-computer/database-url \
  --organization acme --principal validator
```

以独占创建方式写入权限 `0600` 的新凭据文件，不覆盖已有路径。stdout 仅输出凭据 ID 和元数据，不输出 bearer 秘密。每次签发产生不同凭据；轮换流程为签发新凭据、切换调用方，再撤销旧 ID。中断可能留下不可用凭据或不完整文件，重试前检查数据库并撤销未使用 ID。目前没有远程凭据管理或自动轮换。

令牌格式为 `acsk_<128 位随机 ID>_<256 位随机秘密>`。随机字节来自操作系统；数据库只保存有域分隔的 SHA-256 摘要，不保存令牌原文。秘密类型的 Debug 输出脱敏，且不实现 Serialize；摘要使用常数时间比较。依据见 [getrandom](https://docs.rs/getrandom/0.4.3/getrandom/fn.fill.html) 与 [subtle](https://docs.rs/subtle/2.6.1/subtle/trait.ConstantTimeEq.html)。

有效期为 1–86400 的整秒数，以数据库时间判定到期。每次受保护请求检查令牌、到期、撤销、主体状态及准确 scope，静态验证在构造响应前再次检查。`definitions.validate` 仅允许静态验证；`definitions.manage` 允许调用 plan/apply API，但还需独立的声明/引用 grant，不隐含 validate、资源管理或运行权限。禁用主体后，其全部凭据在后续检查中失效；缓存的身份值不能成为后续事务的授权许可。已通过最后检查的请求仍可能结束；SSE/WSS 尚未提供，因此当前没有持续流撤权实现。

## 09.3 HTTP 契约

| 方法/路径 | 认证 | 行为 |
| --- | --- | --- |
| `GET /health` | 公开 | 进程存活，不依赖数据库 |
| `GET /ready` | 公开 | 数据库可达且迁移历史/校验和与内嵌版本完全一致 |
| `GET /v1alpha1/version` | 公开 | 构建与 API 版本 |
| `GET /v1alpha1/capabilities` | 公开 | 实际可用及未支持能力 |
| `GET /v1alpha1/openapi.json` | 公开 | [OpenAPI 3.1 契约](../../schemas/openapi-v1alpha1.json) |
| `POST /v1alpha1/definitions/validate` | Bearer + `definitions.validate` | ComputerSet JSON 静态验证，不写入或执行资源 apply |
| `POST /v1alpha1/plans`、`GET /v1alpha1/plans/{id}` | Bearer + `definitions.manage` + 声明/引用 grant | 创建/读取不可变计划；[详细说明](10-plans-and-apply.md) |
| `POST /v1alpha1/plans/{id}/apply`、`GET /v1alpha1/operations/{id}` | Bearer + `definitions.manage` + 声明/引用 grant | 原子发布资源版本，查询 operation 当前元数据 |

令牌只放在单个 `Authorization: Bearer …` 请求头中。Cookie、查询字符串、调用方请求头和 body 主体字段均不构成身份。当前端点拒绝浏览器 Origin 头，人的 OIDC/CSRF/嵌入登录待实现。正文使用未压缩的 `application/json`，最多 1 MiB，并继续执行重复键、深度/节点和语义限制。YAML 仍用于本地 CLI。有效声明返回 200，无效声明返回 422，`details.validation` 包含诊断。静态验证属于查询，不要求 Idempotency-Key。

错误使用 `code/message/retryable/request_id/details`：无效凭据 401、缺 scope 403、数据库异常通用 503、未知端点 404、错误方法 405、无法读取正文 400、超限 413、媒体格式不支持 415、请求超时 408。请求 ID 由服务生成，忽略调用方传入值。响应使用 `Cache-Control: no-store`；错误消息不回显提交值、数据库 URL 或令牌。

请求处理器最多接受 64 个并发请求，超时 10 秒，最多同时执行 8 个阻塞验证任务。即使请求取消，验证任务仍持有限额直至解析结束；过载返回 503。这些是初始限制，不是吞吐或生产可用性承诺。

## 09.4 验证与剩余范围

operation 进度、数据库协调及本地 `reconciliation-inspect/resume/abandon` 命令见 [11 持久化协调](11-reconciliation-coordination.md)。

共享 `crates/test-support` 为 store/server 测试启动真实、私有 PostgreSQL 集群。3 个凭据场景覆盖随机签发、仅存摘要、主体绑定、scope 分离、篡改、到期、撤销、禁用、时限约束与就绪检查。7 个服务场景覆盖先认证后解析、身份伪造、无副作用验证、重复请求头、协议限制、依赖故障、私有运维文件，以及独立服务进程经 TCP 的签发/验证/授权/目录管理/plan/apply/撤销/SIGTERM。运行方式见[数据库版 Bazel/Cargo 测试](08-persistence.md)。

[声明 grant 和事务内 plan/apply](10-plans-and-apply.md)已实现。OIDC、组织成员和运行时 Workspace/Computer/App grant、ConnectionSession/ViewerSession、受保护流、部署和 Computer 运行时仍待实现。本增量不等于完整 T10/T18/T22 通过，也不代表生产安全认证。
