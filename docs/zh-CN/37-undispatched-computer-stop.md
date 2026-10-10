# 37. 用户派发前的 Computer 停止

## 37.1 支持的生命周期

`POST /v1alpha1/computers/{id}/stop` 现在可以停止已经准备完成、从未派发过用户写入的 Candidate。调用要求服务凭据包含 `runtime.manage`、拥有该 Computer 的精确 `manage` 授权、提供幂等键，并使用 `GET /v1alpha1/computers/{id}/runtime` 返回的当前控制 revision：

```json
{
  "expected_revision": 4,
  "request_id": "start_example"
}
```

准备过程会推进控制 revision，因此原始 start 准入回执中的 revision 已不是最新值。Queued 请求仍使用 `/start/cancel`；存储准备开始后的 Preparing 请求不能通过这两个接口停止。

停止事务检查全部 writer epoch。任何用户派发记录都会阻止该接口，包括旧 epoch 中已经完成的有界文件保存：保留这些更改需要 checkpoint 发布，本轮尚未实现。未释放的 writer、排队执行、仍未到期且报告 active 输入的人类连接也会阻止停止。Idle 连接可以保留。调用方应先关闭或置闲活动连接，取消排队执行，并释放或协调从未派发过写入的 writer，再重试。租约到期本身不等于写者已释放。接口不接受 force 参数或调用方自报的 fencing 证明。

## 37.2 回执、重启与资源预留

停止与会话活动、writer 获取及派发共用组织事务锁。迁移 20 保存不可变停止回执，约束 `Prepared` → `Stopped` 转移，并禁止旧启动请求再次获得 Held lease。回执、控制 revision 与 `computer.stopped` 事件/outbox 原子提交，失败时一并回滚。提交前及重试时重新检查授权。

回执绑定 Computer、原始启动请求、generation、Candidate、固定输入版本及摘要、保留的存储预留、新控制 revision、时间与事件序号，证明类型为 `no_user_dispatch`。当前 generation 已停止且没有活动请求时，运行状态查询会包含 `stop_receipt`。重放原停止幂等键只返回历史回执，不会停止后续 generation；同一键携带不同输入会冲突。

停止清空活动请求，释放活动 Computer/Workspace、主体数量、CPU、内存和运行时长预算预留。旧 Candidate 目录、准备身份、writer 历史及完整存储预留继续保留，不执行存储清理。随后普通 `/start` 准入使用新的控制 revision，分配新的 generation 和 Candidate ID，固定当时已提交的 Workspace 输入，绝不复用旧目录。

存储容量必须同时覆盖保留目录和新 Candidate。当前每个 Candidate 预留 10 GiB，因此 10 GiB Volume 在 prepared stop 后无法重启，20 GiB Volume 可容纳一次替换。在可验证 GC 实现前继续采用此保守计费方式。替换启动准入失败不会撤销之前已完成的停止。

## 37.3 范围与验证

该路径证明没有用户工作负载获得过派发权限，不终止运行中的 sandbox，不排空 JuiceFS 写入，不发布新 checkpoint，也不回收 Candidate。Computer 的 `ready` 仍为 false。完整普通/强制停止、空闲策略、派发后的恢复、多节点 fencing，以及产品 T01–T43 验收仍待完成。

PostgreSQL 契约覆盖 WAL 重启、证据不可变、当前授权、提交前凭据到期、outbox 故障回滚、并发获取 writer、旧 epoch 派发、活动人类输入、Queued/Preparing 拒绝、已有派发历史的迁移、新 generation 与旧存储配额保留。HTTP 契约覆盖 scope、严格请求结构、Origin 拒绝、错误响应、当前停止回执及授权重试。这些测试中的准备和文件回执为合成数据，仅验证数据库权限边界；本轮不声称新增 Kubernetes/CSI 运行实验。

2026-10-10 的[源码绑定验证记录](../evidence/undispatched-computer-stop-2026-10-10.json)及[日志](../evidence/undispatched-computer-stop-2026-10-10.log)记录 338 个 Cargo 测试通过（含 151 个 PostgreSQL 用例），11 个 Bazel 测试目标通过，其中新增 11 个停止边界用例。格式、Clippy、文档和现有 Qualitygate 策略检查均通过。
