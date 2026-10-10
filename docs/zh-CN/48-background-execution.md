# 48. 有界后台执行

新执行请求默认采用 `lifetime: background`。逻辑连接关闭后，已提交任务仍持有原写入 epoch 的预留。显式 `lifetime: connection` 保留此前行为：关闭连接会降低写权限，缺少完成证据的已派发任务进入 Unknown。执行元数据返回不可变的 lifetime，幂等摘要也包含该值。

```json
{"lease_id":"writer_example","lease":{"connection_session_id":"session_example","generation":1,"epoch":1,"expected_revision":1},"sandbox_id":"sandbox_example","command":{"argv":["/bin/sh","-c","printf saved > result.txt"],"cwd":"","timeout_seconds":10,"term_grace_ms":500,"output_limit_bytes":4096}}
```

## 权限与期限

断开策略本身不延长期限。固定 v1 执行继续使用至多 30 秒、且被准备过程消耗的原预算。新请求可独立采用[可续期执行租约](49-renewable-execution-leases.md)，只有原受信任 worker 能授权并确认续约。

Closed 连接没有能力，不能续租、取得写租约、保存文件或提交新任务。重新连接不能抢占已预留 epoch 或改变期限。原凭据仍有效时，可查询、重试或取消其拥有的执行；新凭据不会继承这种所有权。显式取消及 `cancel_running=true` 的检查点停止仍会取消后台任务，取消请求本身不证明进程已停止。

只有精确、不可变的后台预留可以替代逻辑连接 Active 检查。派发、启动、观察及完成仍检查原主体、凭据、scope、Computer/Workspace grant、catalog、generation、Candidate 身份和固定期限。撤权仍使写者进入 Draining，未确认派发保持 Unknown。接受完成和释放写租约仍需活的进程/IO 证明及已验证输出；重启不重建证明，也不重放派发。

关闭连接的事务只保留尚未完成的后台预留。已取消且未派发的任务不能在关闭后继续占用写者，旧后台历史不能授权新 epoch。后台身份谓词包含终态历史，是因为成功完成在记录结果后、同一事务内还会复核权限；这不会绕过写者状态、期限或排空检查。

## 兼容与验证

迁移 28 约束存储中的 lifetime 值，增加精确组织/lease/epoch 谓词。已有显式 `connection` 输入、摘要和回执不变。缺少 lifetime 的旧响应元数据按 connection 读取；新请求省略 lifetime 则按 background 处理。能力报告 `execution.admission: bounded-queued`；公开 `execution` 仍不支持，Computer ready 仍为 false。

数据库和 HTTP 回归覆盖派发前后断开、WAL 重启、Outbox 回滚、重试冲突、模式不可变、到期、凭据/主体/grant/catalog 撤销、新写入拒绝及检查点取消。真实 gVisor 场景分别在派发前关闭后台连接，以及看到已刷盘启动标记且输出文件尚不存在时关闭连接；验证接受完成、复用同一活证明的完成重试、文件保留及鉴权输出下载。第三个断开的任务经显式取消后生成检查点并恢复。独立 SQL、签名 S3 读取及新的只读 JuiceFS 客户端验证结果。

[源码绑定记录](../evidence/background-execution-2026-10-10.json) · [验证日志](../evidence/background-execution-2026-10-10.log)

证据限于一台临时 Linux VM 和现有有界集成链路。完整生命周期调度、App/browser 健康与 checkpoint、自动排空恢复、跨节点 fencing 及 T01–T43 产品验收仍未完成。
