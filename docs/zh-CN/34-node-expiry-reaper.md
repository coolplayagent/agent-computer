# 34. 节点常驻到期回收

## 34.1 恢复已有终止意图

[双节点 watchdog](32-redundant-node-watchdogs.md)仍负责原固定截止时间。新增独立 root 服务扫描[持久日志目录](33-watchdog-journals.md)，使两个守卫退出或停止后仍可恢复原终止意图。服务重启后继续到期回收，不启动工作负载，也不签发启动授权。

新日志增加不可变 `enrollment.json`。watchdog 先验证当前 boot、打开可信原 cgroup，再写入登记；登记把日志设备/inode、intent 精确摘要和 cgroup 设备绑定。文件和目录同步在内核定时器设定及 armed 回执之前完成，设置耗时占用原截止预算。节点适配器要求登记匹配才接受活守卫。没有登记的历史或未完成初始化日志仍可读取，但服务不会由此推定操作权限。

服务只处理配置私有 spool 中的 `journal-*` 目录。到期后检查登记、intent 摘要、当前 boot、原 cgroup 路径/设备/inode、委派权限、domain 类型，以及目标不是服务自身或祖先 cgroup。设备验证在构造 drop 时尽力终止的 cgroup 句柄之前完成。服务不会向日志中的旧 PID 发信号。旧 boot、路径替换、记录无效或 cgroup 不可用不能产生成功回收报告；已有空树报告保持原样。

执行 `cgroup.kill` 后仍有进程的树保持未确认，后续扫描重试，不逐条等待不可中断进程五秒而阻塞其他条目。内核确认空树后，同步独立不可变 `recovery.json`，标记 `trigger: Recovery`。临时名称唯一，写入中断后可继续尝试，未完成文件不算成功；原 `intent.json` 与 `report.json` 不被覆盖。Linux [cgroup v2 契约](https://docs.kernel.org/admin-guide/cgroup-v2.html)规定递归 kill 与 populated 观察，它们不能封闭将来的成员加入，也不能证明存储客户端排空。

## 34.2 服务生命周期与观察

服务对 spool 目录持排他锁；第二个回收器失败，不竞争最终报告，独立守卫不使用该锁。游标以每批最多 128 个条目遍历整个目录，每轮完成后等待 250 ms 再扫描。日志数量和存储延迟影响扫描时间；分批限制内存，不限制文件系统调用耗时。spool 必须位于可信节点本地持久存储，保留未解决记录，活动期间不能移动或替换。自动保留和容量管理尚未实现。

仓库提供的 [systemd unit](../../deploy/systemd/agent-computer-expiry-reaper.service)使用 `Type=notify`、`Restart=always`、250 ms 重启间隔和十秒服务 watchdog。扫描循环发送 readiness 与 watchdog 通知，不依赖 stdout 或 journald 吞吐。服务管理器可替换崩溃或停止的进程，watchdog 超时使用 SIGKILL。语义见上游 [systemd service 契约](https://raw.githubusercontent.com/systemd/systemd/v255/man/systemd.service.xml)。ready 仅表示 spool 和锁已打开，不代表启动授权或全部日志健康。

使用 `systemctl show agent-computer-expiry-reaper.service -p ActiveState -p SubState -p StatusText -p NRestarts` 查看服务状态及当前扫描轮的不可用条目数量。`agent-computer-watchdog --reap-once --spool PATH` 是**会修改运行状态的运维命令**：完整扫描一轮，可能终止过期目标，输出有界 JSON 批次；记录不可用或无法取得锁时退出 2，不能与服务同时运行。常规 `execution-recover-once` 对日志仍仅做观察。

恢复输出新增 `watchdog_journals.guards[].state: recovered`，表示原守卫没有报告，但回收器提供了匹配报告。如果原报告也存在，`recorded` 保留它，并可附加独立 `recovery` 字段。读取仍核对原数据库 arm、请求、原守卫 PID 和 cgroup 设备。回收报告中的 `armed_boottime_ms` 是本次回收开始时间，不是重建原 armed 回执。

## 34.3 运维安装

服务必须位于主机命名空间、工作负载 cgroup 之外，并与执行 worker 使用同一节点和 spool。安装由运维显式执行，库和 worker 不隐式部署服务。替换配置二进制前先排空活动派发。构建并审查固定产物后执行：

```bash
sudo install -o root -g root -m 0755 \
  bazel-bin/crates/watchdog/agent-computer-watchdog /usr/local/bin/agent-computer-watchdog
sudo install -o root -g root -m 0644 \
  deploy/systemd/agent-computer-expiry-reaper.service \
  /etc/systemd/system/agent-computer-expiry-reaper.service
sudo systemctl daemon-reload
sudo systemctl enable --now agent-computer-expiry-reaper.service
sha256sum /usr/local/bin/agent-computer-watchdog
systemctl show agent-computer-expiry-reaper.service -p ActiveState -p SubState -p StatusText
```

unit 创建 root 所有、模式 `0700` 的 `/var/lib/agent-computer/watchdogs`。执行 worker 的 `node.spool` 必须使用相同路径，`node.watchdog.sha256` 更新为实际二进制摘要。采用其他 spool 时同步修改 unit 路径及挂载依赖，启动前准备等价私有目录权限。不能向租户开放 spool、二进制调用或服务管理。本轮组件环境为 Linux 6.8 和 systemd 255，其他配置需另行验证。

## 34.4 验证与边界

root VM 场景覆盖双守卫被杀死/停止、服务 SIGKILL 和 SIGSTOP 后 systemd 实际替换、报告发布中断、重复回收器互斥、设备/inode/摘要拒绝、旧 boot、未登记日志、路径复用、非私有 spool 拒绝，以及 302 条目的分页扫描。节点 fixture 另通过原双 arm 引用读取回收结果，并拒绝伪装成原 deadline 报告的回收记录。原双守卫、日志 IO 和内核探针继续作为回归验证。

这是故障后回收机制，不能承诺硬实时截止：服务 watchdog 先要发现停止的进程，磁盘/内核阻塞或扫描积压也可能延迟终止。原双守卫仍是必需条件，服务可用性尚未接入数据库启动准入。本轮组件测试没有认证整机重启、断电持久性、多节点路由、存储 fencing 或完整 Kubernetes/CSI/worker 链路。任何报告均不释放 writer 或接受执行输出；Execution 保持 Unknown、writer 保持 Draining，产品 T01–T43 仍为 `not_run`。
