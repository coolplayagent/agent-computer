# 35. 到期回收服务的启动准入

## 35.1 绑定正在响应的服务

节点 guard 现在必须确认[常驻到期回收服务](34-node-expiry-reaper.md)正在响应，才能授权新启动。两个独立 watchdog 先持久化 enrollment，并布防原始固定期限；节点随后向同一私有 spool 内的 `reaper.sock` 发送新的 256 位随机挑战。服务核对两个准确日志引用、已登记请求与 cgroup device、两个不同的原始 guard PID、当前 boot 与 cgroup 身份、尚无最终报告及期限仍有效。探测 cgroup 不会构造析构时执行 kill 的句柄。

回复绑定 nonce、请求、两个日志、cgroup device、服务实例/PID、spool device/inode 和观察时间。服务每次启动生成新的随机实例标识，在持有 spool 排他锁期间创建端点，再报告 readiness。请求由到期扫描循环处理；没有独立心跳线程在扫描阻塞时继续回答。每批最多处理八个请求，并在现有 250 ms 扫描间隔内继续轮询。报文限 4 KiB，socket IO 为非阻塞。

这是可信宿主机本地 IPC。root 所有的私有目录与 socket 阻止租户访问，已连接的 datagram 客户端只接收选定端点的回复。它不是签名证明，也不防御已被攻陷的宿主机 root。spool、运行时和节点二进制仍须处于既有宿主机信任边界内。

## 35.2 新鲜检查与不可恢复的句柄失效

已有的每个 `ArmedGuard::remaining_budget_ms` 检查点现在都会先发送新挑战，再检查原始 guard 进程并计算剩余预算，覆盖布防登记、启动授权和后续运行时检查。客户端要求整个句柄生命周期内的服务实例、PID 与 spool 身份保持一致。任何探测失败都会永久作废该客户端；服务恢复或重启不能复活它，序列化回执也不能重建 live guard。

成功探测必须在 200 ms 内完成，报告时间位于挑战开始与收到回复之间，且在原执行期限前完成。探测消耗原预算，不重置期限。文件系统调用仍为同步且不可取消，因此 200 ms 是接受回复的界限，不是文件系统阻塞的硬时限。Store 目前在事务内同步调用本地探测，慢存储或不可用存储可能延迟事务。服务也可能在成功回复后立刻失败；独立 guard 对与可重启到期服务继续承担各自职责。

migration 18 将初始回执绑定到不可变 arm 及两个日志引用，检查字段格式和观察新鲜度，拒绝同一 boot 上重复的实例/nonce，并要求新 planned-Pod 启动 grant 带有 reaper 证据。存储的 JSON 是审计元数据，不代表当前存活。迁移保留历史行，不补造授权：缺少 reaper 证据的历史 arm 不能产生新的 planned-Pod grant。

## 35.3 部署与验证

按[已有步骤](34-node-expiry-reaper.md#343-运维安装)安装 [systemd unit](../../deploy/systemd/agent-computer-expiry-reaper.service)，spool 必须与 worker 的 `node.spool` 完全一致。替换二进制时排空派发，并更新固定 hash。新派发前必须启动服务。端点缺失会在 Watchdog 阶段失败，不登记 arm、不创建 grant、不执行用户命令，worker 保持 Unknown/Draining。即使后续设置失败，已布防的独立定时器也保留原始期限。

验证覆盖回复重放、身份移植、过期拒绝、PostgreSQL 元数据约束与历史迁移。root fixture 检查服务缺失/暂停拒绝、正常探测不终止目标、重复日志拒绝、重启实例变化和失败后永久失效。真实单节点 K3s/CSI/gVisor worker fixture 在执行、取消、controller/PID 1 故障场景之外加入服务缺失拒绝。这些是组件证据，不代表产品 T01–T43 完成。

服务仍只执行终止意图。服务存活与空 cgroup 都不能证明存储 fencing、writer drain、输出接受或权威完成；这些仍需后续实现。
