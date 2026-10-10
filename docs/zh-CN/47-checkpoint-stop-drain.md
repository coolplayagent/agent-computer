# 47. 检查点停止的显式执行取消

`checkpoint-stop` 可用可选的 `cancel_running=true` 请求排空当前 Candidate。省略该字段或设为 false 时，继续要求写者已排空，并保留已有请求的幂等摘要。

```http
POST /v1alpha1/computers/{id}/checkpoint-stop
Authorization: Bearer <credential>
Idempotency-Key: <stable-key>
Content-Type: application/json

{"request_id":"start_example","expected_revision":4,"publish_current":true,"cancel_running":true}
```

## 状态与证据

准入在同一事务中写入 Artifact `Draining`、Computer 启动状态 `Draining`、控制 revision 和事件/outbox，取消尚未派发的执行，并将已派发执行置为 `CancelRequested`。Prepared 边界随即关闭：不能获取或续租写租约、提交新执行、开始文件修改或获得新启动授权。原始执行只能继续确认取消，不获得新的执行权限。

取消请求不是停止证明。未派发工作沿用 `no_dispatch`，有界文件工作沿用封闭后的文件完成证明，派发执行必须由原可信 worker 提供活的进程与 I/O 排空证明。执行身份、凭据、权限、原截止时间和存储身份检查继续生效。原 worker 丢失、授权失效或原预算耗尽可能保留 Unknown；重启、Pod 删除、到期或更换发布凭据都不能补造结果。

常驻 Artifact worker 发现 Draining 工作，复核当前发布权限和活跃使用，并只收集现有排空证明。证明不足时不领取捕获租约，保持 `Draining`。元数据 `drain_reason=drain_pending` 表示仍在等待；若存在 Unknown 则为 `recovery_blocked`，即使物理写者已释放也不能捕获。证明完整后，原子转为 Artifact `Capturing` / 启动 `Sealing`，控制 revision 再增加一次，然后沿用原捕获、S3 全量验证、发布与停止原子事务。

只有 `stop_receipt` 确认停止；`202`、`CancelRequested` 或上传成功均不确认停止。`publish_current` 的 CAS、分支和冲突语义与[检查点停止](46-checkpoint-stop-worker.md)相同。已停止历史重试不会停止新 generation。原主体可以用新凭据重试原 key 和相同模式，但不能改变 `cancel_running` 或改变原执行身份。

## 授权与迁移

仍要求 Computer read/modify/manage、Workspace read/modify/publish 和对应 scope。未实现 App/profile 捕获，任何已声明 App 都阻止准入。其他主体的有效 Active 连接或活跃人的输入也阻止普通停止；准入后出现的活跃使用会阻止捕获晋级及最终停止。此选项不提供 force。

迁移 27 保留原 Artifact 身份和幂等输入，历史 `cancel_running` 默认为 false。数据库约束拒绝跳过 Draining、没有排空证明的封存、待排空时领取捕获租约、模式改变或不匹配的 runtime 状态。迁移测试恢复旧版本函数与约束，再验证升级；旧版历史对比显式排除新增的默认列。

## 验证与限制

数据库和 HTTP 回归覆盖取消事务与 outbox 回滚、WAL 重启、模式幂等冲突、旧凭据撤销与新发布凭据重试、Unknown 阻塞、App/活跃使用限制、新写入拒绝和竞争领取。真实 VM 组件测试在 gVisor 进程写出完整 `started.txt` 后请求停止，确认排空前无法捕获，接受原执行的取消完成，再发布并恢复到新 Candidate。独立 SQL、签名 S3 GET 与无缓存的只读 JuiceFS 客户端验证文件、回执和独立 inode；`late.txt` 不存在。

[源码绑定记录](../evidence/checkpoint-stop-drain-2026-10-10.json) · [验证日志](../evidence/checkpoint-stop-drain-2026-10-10.log)

范围为单节点的已接入执行链路。一般进程排空、丢失控制器后的自动恢复、跨节点 fencing、App/浏览器 checkpoint、GC 与完整生命周期调度尚未完成。Computer `ready=false`、公开执行能力不支持，T01–T43 的产品验收状态保持不变。
