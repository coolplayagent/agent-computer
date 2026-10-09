# 21. Candidate 文件 HTTP 网关

## 21.1 启用有界网关

控制服务可向服务凭据客户端开放已准备 Candidate 的文件。启动参数为 `serve --database-url-file ... --file-config /run/secrets/agent-computer/file-gateway.json`。私有 JSON 文件是 1–16 项[文件 worker 配置](20-bounded-file-saves.md)的数组，每项包含 `mount_root` 与准确的不可变准备 `target`；拒绝重复 Volume ID 和相对挂载根路径。现有私有文件读取器将配置限制在 8192 字节。挂载根及其祖先由运维者控制，用户不能指定宿主路径或存储绑定。

不传该参数时，`files.read`、`files.save` 继续为 `unsupported`；启用后能力响应为 `bounded-candidate`。每次请求仍需匹配配置的 Volume 和符合要求的实际 JuiceFS 挂载。配置不等于挂载健康或 Computer Ready。TLS 终止仍由部署提供；独立浏览器认证流程实现前拒绝带 Origin 的浏览器请求。

## 21.2 使用独立连接读取

`GET /v1alpha1/workspaces/{id}/files` 要求查询参数 `connection_session_id`、`generation`、`candidate_id`、`path` 各出现一次，拒绝未知参数。当前只读单个文件，未实现目录列表和分页。

```text
GET /v1alpha1/workspaces/workspace-example/files?connection_session_id=connection-example&generation=1&candidate_id=candidate-example&path=note.txt
Authorization: Bearer <original-connection-credential>
```

凭据需要 `runtime.connect` 和 `runtime.read`。连接必须绑定该准确凭据、处于有效活动状态且请求过 read 能力；同时要求当前 Computer connect/read grant 和独立 Workspace read grant。不要求 modify 或写入租约。服务在 IO 前校验当前 generation、Candidate、准备绑定和固定目录引用，返回字节前再次鉴权并比较绑定。等待文件系统时不持有数据库事务。已披露的字节不能因后续撤权而收回。

成功返回 JSON：`version` 包含 `sha256`、`size`、`executable`，`content` 为字节数组。读取上限为 1 MiB，并固定普通文件 inode；网关原子替换期间可读到旧版本或新版本。拒绝路径逃逸、符号链接、硬链接、FIFO、跨挂载、保留暂存名及被替换的 Candidate 根目录。缺失或不支持的文件对象返回 `404 file_unavailable`。该保证要求所有写者经过 Candidate 网关，不覆盖非受管进程原地改写。

## 21.3 保存与查询不确定结果

`POST /v1alpha1/leases/{id}/file` 接受与[第 20 节](20-bounded-file-saves.md)相同的严格 JSON：`lease`、稳定 `dispatch_id` 和 `edit`。请求必须是未压缩 JSON，最多 5 MiB，以容纳最多 1 MiB 文件的字节数组编码。`edit.expected` 使用先前读取的内容版本，null 要求目标不存在。修改仍要求 Computer 和 Workspace 的 read/modify grant，以及原连接当前所有的写入租约。

该端点使用稳定 `dispatch_id` 作为副作用身份，不使用单独的 Idempotency-Key 请求头。准确重试仅返回记录，不执行 IO；同一 ID 改换内容、路径或前提返回冲突；派发日志尚无完成记录时返回 `409 dispatch_unresolved`，绝不重发写入。超时后查询 `GET /v1alpha1/leases/{id}` 或使用同一意图重试，不得换新派发 ID 绕过不确定状态。进入后续 epoch 后，旧 epoch 的重试返回冲突。

`200` 响应是租约元数据，不等于无条件成功；须检查 `file_edit.state` 的 Applied、Conflict、Expired 或 Unknown。已有排空规则继续生效，不新增接收客户端自报排空的 HTTP 接口。文件字节不复制到数据库、事件或 Outbox。

## 21.4 容量、取消与验证

每个已配置网关共享四个 IO 名额，保存请求读取正文前即预留名额，耗尽时返回 `503 file_io_busy`。文件任务独立于 HTTP 等待 future 运行；现有十秒 HTTP 期限不会释放任务名额、取消 FUSE 调用或宣布写者排空。阻塞的挂载/文件调用在 Tokio 异步 worker 之外执行，直到 IO 与结果接纳实际返回才归还名额。全局 64 个 HTTP 请求名额与四个保留的 IO 名额分开计数，因此四个阻塞文件任务不会占尽控制请求容量。不自动重试未解决的文件副作用。

新增两项存储场景覆盖有界读取与恶意对象；五项 PostgreSQL 场景覆盖独立读取 grant/scope、准确连接凭据、已准备代次/目录绑定、关闭/到期/撤权及请求能力；四项服务场景覆盖显式启用、请求限额、未知/重复查询字段、浏览器拒绝，以及等待方取消后保留 IO 名额且就绪接口仍可响应。默认测试现为 224 项。显式真实 Candidate 组件测试另运行独立 TCP 服务，覆盖只读会话读取、保存、准确/改换意图重试、跨凭据拒绝、符号链接拒绝、Workspace 撤权与连接关闭；另注入十一秒 Outbox 延迟，验证 HTTP 超时后查询已完成任务、准确重试不再写入。执行证据单独记录。

2026-10-10（Asia/Singapore），显式组件测试在 `e7500e7` 上通过。[固定记录](../evidence/candidate-file-http-2026-10-10.json)与[运行输出](../evidence/candidate-file-http-2026-10-10.log)绑定实际部署的 Bazel 二进制和 174 个源码文件。上述 HTTP 场景均通过，包括先收到 408、随后查询 Applied/Released，准确重试保留 inode。组合测试共记录八条派发/完成/事件/Outbox 和七条有界排空证明。新的只读 JuiceFS 客户端读回最终 19 字节 HTTP 保存文件，SHA-256 与记录一致；连同准备恢复文件，两次 S3 GET 共读取 46 字节。一次性虚拟机和私有状态已删除。延迟数据库事务的测试不代表 FUSE 挂起或物理 fencing 已验证。

目录操作、大文件流、OIDC/人的浏览器会话、文件 UI、产品 Pod 执行、通用进程 fencing、Artifact 发布与 Computer Ready 仍待实现，T01–T43 保持 `not_run`。
