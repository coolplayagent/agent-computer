# 46. 持久检查点停止与 Artifact worker

无 App 的 Computer 现在可以通过一个持久请求保存当前文件并停止。请求先封存已排空的 Candidate；后台 Artifact worker 捕获并完整验证 S3 对象，再在同一数据库事务中发布检查点并记录停止。请求被接受仍表示工作待完成。Computer Ready、App/profile 状态捕获及通用进程排空仍是独立要求。

## 请求与授权

```http
POST /v1alpha1/computers/{id}/checkpoint-stop
Authorization: Bearer <credential>
Idempotency-Key: <stable-key>
Content-Type: application/json

{"request_id":"start_example","expected_revision":4,"publish_current":true}
```

调用者需要 Computer read/modify/manage 与 Workspace read/modify/publish grant，以及对应凭据 scope。服务从已准入的启动请求读取原 Workspace 输入 revision 和 manifest。调用者不能提交存储事实、停止回执或 force 标记。`202` 返回 Artifact `commit_id`、`state` 和 `stop_after_commit=true`；继续查询现有 Artifact 接口或 Computer runtime，只有已提交的 `stop_receipt` 才确认停止。

准入要求 Candidate 为 Prepared，全部写者已释放，全部已派发效果满足已有排空/完成证明。Unknown 执行结果仍阻止检查点，即使物理写者已释放。由于尚未实现 App/profile 捕获，Computer 必须未声明任何 App。其他主体未过期的 Active 连接，或任何人的活跃输入（包括申请者本人），都会阻止普通停止；申请者自己的空闲连接可以保留。活跃使用返回 `409 active_use`，不披露其他身份。准入后出现新的活跃使用会阻止最终提交，已封存 Candidate 和已捕获内容继续保留，供之后重试。

`publish_current=true` 用原 Workspace 基线 revision 做 CAS；若共享指针已变，保留 Conflict artifact，并用该固定版本完成停止，不覆盖新指针。`false` 保存分支检查点。普通后续启动选择当前 Workspace 指针；`input_artifact_id` 可明确选择检查点。两者都会分配新 generation 和独立 Candidate，旧 Candidate 文件及容量预留继续保留。

请求、原主体和 `stop_after_commit` 模式不可变。原主体可以用重新授权的凭据重试相同 key 与输入，使旧 worker 租约失效而不改变已捕获文件。领取、续租和提交都会检查当前权限，事件/outbox 写入后也再次检查。重试已完成操作返回历史结果，不能停止新一代实例。

## 发布与停止

迁移 26 增加停止模式与待处理索引。Artifact worker 复用现有捕获和完整对象验证规则，在同一事务内将 Sealing 转为 Sealed、构造现有纯文件检查点回执、将启动置为 Stopped 并清空 Computer 活动请求。Artifact 与停止事件使用相邻序号。延迟数据库约束拒绝没有匹配停止回执的检查点发布。事件失败、租约过期或授权丢失会一起回滚发布、共享指针推进与停止；已经上传的不可变对象可以安全重试。

已有显式 Artifact 发布与立即停止接口继续保留各自语义。本操作要求写者已排空，不取消正在运行的任务，不先接受请求再等待活跃使用结束，也不实现管理员强停。[执行完成接受](42-accepted-execution-completion.md)的证明可以使成功、失败或已取消执行的历史满足捕获条件；仅有 cgroup 为空、Pod 消失或租约过期不足以做到这一点。

## 常驻 worker

```sh
agent-computer-server artifact-worker --database-url-file /private/control-url --organization example --worker-id publisher --config-file /private/artifact-worker.json --concurrency 2 --poll-ms 1000
```

私有配置与 [artifact-publish-once](38-workspace-artifact-checkpoints.md) 相同：`storage` 包含合格 target 与既有挂载根，`spool` 位于工作负载挂载之外，`objects` 为 S3 客户端配置。常驻进程先打开这些本地资源再发现任务，不供应或挂载 Volume。已有捕获记录仍可通过单次命令恢复，无需重新读取 Candidate 文件。

只读发现限定精确组织和完整存储身份，排除活跃 worker 租约及已完成提交。每个任务仍经过原有原子领取和当前授权。worker 按 ID 轮换可处理任务，对 Busy/未确认工作进行 5 秒本地冷却，每次轮询最多调度一个任务。共享边界为 1–4 个并发已调度任务、250–5000 ms 轮询；此命令默认单槽、1000 ms。

SIGTERM/Ctrl-C 停止新调度并等待已调度工作，包括正在等待数据库领取的任务。若文件系统捕获线程仍在运行时续租失败，该槽会保留到线程结束，不使用失效租约发布结果。异步上传/提交失败会保留同一不可变捕获供重试。强制退出保留封存目录与持久身份。同步日志和阻塞文件 I/O 可能延迟退出。[systemd 示例](../../deploy/systemd/agent-computer-artifact-worker.service)使用 360 秒停止超时；这不是物理 I/O 期限，也不是整个部署的并发上限。

## 证据与剩余范围

测试覆盖授权、普通活跃使用检查、发布/停止原子性、迟到过期、outbox 回滚、凭据替换、固定输入选择、WAL 重启、历史重试、迁移、精确发现及竞争领取。线程屏障验证续租失败不会遗留捕获线程。HTTP 测试覆盖严格请求体、准入与授权元数据。

临时 VM 在 S3/发布失败并删除本地 spool 后运行真实常驻进程，在领取等待 SQL 锁期间发送 SIGTERM，验证等待完成的检查点停止结果和空重启。独立的真实 gVisor 执行夹具验证执行文件经检查点停止保存，再恢复到新的 Candidate/缓存。独立 SQL、S3 与新建只读 JuiceFS 客户端回读复核结果。

[源码绑定组件记录](../evidence/checkpoint-stop-worker-2026-10-10.json) · [验证日志](../evidence/checkpoint-stop-worker-2026-10-10.log)

这些是单节点组件结果。活动进程排空、强停、App/浏览器检查点、完整生命周期调度、跨节点 fencing、GC 与产品认证尚未完成。Computer 保持 `ready=false`，公开执行能力仍不支持，T01–T43 保持 `not_run`。

验证记录也保留了先前运行中偶发的启动或看门狗拒绝：未签发启动许可、未接受用户文件变更。当前日志未暴露确切的瞬时原因。最终运行在确认所有保留的旧截止时间已过期后，使用新的私有看门狗日志目录；一次完整组件验证不能证明启动可靠性或吞吐能力。

[47 显式执行取消](47-checkpoint-stop-drain.md)扩展此接口；本章的已排空要求描述默认 `cancel_running=false` 模式。
