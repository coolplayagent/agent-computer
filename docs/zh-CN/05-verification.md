# 05. 验证与交付证据

## 05.1 证据分层

| 检查 | 能证明的范围 | 不能证明的范围 |
| --- | --- | --- |
| Bazel build/test | 当前 Rust 目标编译及实际运行的契约断言 | 外部服务、真实隔离和存储耐久 |
| PostgreSQL 集成测试 | 本地事务、并发、重放与 WAL 恢复 | 复制切换、断电/磁盘故障或生产部署认证 |
| Cargo fmt/Clippy | 当前源码格式与静态诊断 | 产品业务验收 |
| 文档校验 | 本地链接、双语文件与序号对应 | 所引用设计已经实现 |
| Qualitygate full | 当前策略选中的交付快照检查 | 策略未选择的测试；当前策略仅行尾 |
| relay-knowledge map validate | 映射结构、路由与摘要完整性 | 索引覆盖完整或运行时正确 |
| T01–T43 运行验收 | 各编号在固定真实环境中的实测行为 | 未执行编号或其它部署组合 |

## 05.2 当前记录

2026-10-09（阶段 01–02、03–07 部分能力）：Bazel 构建、CLI JSON 输出与错误退出码、Cargo fmt/Clippy、本地文档链接与双语编号检查通过；Bazel 共通过 213 项测试（28 领域、28 声明、7 CLI 进程、88 PostgreSQL、13 服务、18 Kubernetes 适配器、31 存储场景）。另以 Python 独立验证 Draft 2020-12 Schema 和示例 SHA-256；Cargo 测试与 Clippy 同样通过。数据库实测环境为 Linux x86_64、PostgreSQL 18.6 私有临时集群，保留耐久配置；覆盖并发幂等/CAS、回滚、不可变版本、迁移校验和、快照/重放一致性、Outbox 保留及 WAL 崩溃恢复。复现命令和限制见 [08 持久化](08-persistence.md)。服务测试还验证独立 TCP 进程的凭据签发、验证、声明授权、目录管理、plan/apply 重试、撤销及优雅退出；存储测试覆盖到期、scope 隔离和主体禁用。OpenAPI 文件通过官方 3.1 元 Schema 和内嵌 ComputerSet 引用检查，见 [09 控制服务](09-control-service.md) 与 [10 声明计划](10-plans-and-apply.md)。计划测试还覆盖七资源发布、依赖版本/权限检查、撤权锁竞争和末尾写入失败的完整回滚，仅发布控制元数据与排队意图。另有十项协调场景验证领取互斥、租约/派发恢复、撤权竞争和原子完成；模拟适配器回执仅是元数据证据，见 [11 持久化协调](11-reconciliation-coordination.md)。Kubernetes 协议测试覆盖响应丢失、身份/spec 冲突、条件删除与传输限额，显式真实组件测试入口见 [12 Kubernetes 适配器](12-kubernetes-adapter.md)。另在提交 `461d52c` 上通过一项显式 Kubernetes/gVisor 组件实测，[固定证据](../evidence/kubernetes-component-2026-10-09.json)记录真实虚拟机、运行时与限制。另有三项数据库和五项卷协议测试，覆盖持久化 PVC/PV 身份记录、类型筛选领取及严格 JuiceFS 回读，见 [13 卷供应](13-volume-provisioning.md)。显式 Volume worker 及独立 gVisor 文件/配额/新缓存读取探针也已通过；[组件证据](../evidence/juicefs-component-2026-10-09.json)保留 `3cf8370` 与 `30213c4` 的不同观测。Candidate 准备测试及真实 JuiceFS 命令探针覆盖独立文件、重试和目录配额，见 [14 Candidate 存储](14-candidate-storage.md)及[固定源码组件证据](../evidence/candidate-storage-2026-10-09.json)。新增 10 项 PostgreSQL 与 1 项 HTTP 场景验证独立运行 grant、激活上限、授权锁竞争和有效 scope 交集；真实 TCP 场景另执行运行授权/撤权命令，见 [15 运行授权](15-runtime-authorization.md)。运行验收 T01–T43 全部 `not_run`。现有设计检查 T00 不等于产品交付。

新增 11 项 PostgreSQL 与 1 项 HTTP 测试验证持久化启动准入、固定快照、容量竞争、取消、回滚及 WAL 恢复，见 [16 启动准入](16-start-admission.md)。

本地 Qualitygate 报告保存在已忽略的 `.qualitygate/`；提交前对最终工作区运行 `check --worktree --profile full`。报告需满足非空交付、无 pending checks、`gate.complete: true`、`gate.decision: pass` 与退出码 0，另外执行 Bazel 和静态检查。保留报告的 snapshot/policy digest，不把默认行尾检查扩大解释为功能质量保证。

运行证据字段沿用[验收记录格式](../../codespec/test/agent-computer.md)：`test_id/status/source_commit/component_digests/environment/input_refs/expected/observed/evidence_refs/limits`。当前不会生成虚假的 `passed` 运行报告。

新增 7 项 PostgreSQL 与 2 项存储测试验证已提交输入绑定、准备认领/派发、收据一致性、撤权、仅观察恢复及迁移安全，见 [17 Candidate 准备 worker](17-candidate-preparation-worker.md)。另在 `f4bff87` 上通过一项真实 worker 手动测试，覆盖实际 Volume 供应、准备命令、回执丢失后保留 inode/文件、缺失发布不重建及新挂载 S3 回读；[固定证据](../evidence/candidate-worker-2026-10-09.json)记录该组件范围，不计为完整 Computer 运行验收。

新增 10 项 PostgreSQL 和 2 项 HTTP 场景验证逻辑连接、当前权限交集、心跳 CAS、终态撤销、凭据/组织隔离及固定有效期；已有 TCP 进程测试也覆盖该生命周期，见 [18 连接会话](18-connection-sessions.md)。

新增 12 项 PostgreSQL 与 2 项 HTTP 场景验证连接所有的 Candidate 写入租约、受控派发、零派发交接、撤权及 Outbox 回滚；已有 TCP 进程场景另执行写入租约对账。准备收据为模拟元数据，不声称物理排空已完成，见 [19 写入租约](19-candidate-writer-leases.md)。

新增 7 项本地存储与 2 项 PostgreSQL 场景验证有界文件保存、未知派发及升级安全；真实 worker 测试另增加文件保存/排空场景，其结果不由默认测试推断，见 [20 文件保存](20-bounded-file-saves.md)。
