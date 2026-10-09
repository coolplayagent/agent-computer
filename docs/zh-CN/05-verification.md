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

2026-10-10（阶段 01–02、03–08 部分能力）：Bazel 构建、CLI JSON 输出与错误退出码、Cargo fmt/Clippy、本地文档链接与双语编号检查通过；Bazel 共通过 288 项测试（28 领域、28 声明、7 CLI 进程、131 PostgreSQL、20 服务、33 Kubernetes 适配器、33 存储、8 监督器场景）。另以 Python 独立验证 Draft 2020-12 Schema 和示例 SHA-256；Cargo 测试与 Clippy 同样通过。数据库实测环境为 Linux x86_64、PostgreSQL 18.6 私有临时集群，保留耐久配置；覆盖并发幂等/CAS、回滚、不可变版本、迁移校验和、快照/重放一致性、Outbox 保留及 WAL 崩溃恢复。复现命令和限制见 [08 持久化](08-persistence.md)。服务测试还验证独立 TCP 进程的凭据签发、验证、声明授权、目录管理、plan/apply 重试、撤销及优雅退出；存储测试覆盖到期、scope 隔离和主体禁用。OpenAPI 文件通过官方 3.1 元 Schema 和内嵌 ComputerSet 引用检查，见 [09 控制服务](09-control-service.md) 与 [10 声明计划](10-plans-and-apply.md)。计划测试还覆盖七资源发布、依赖版本/权限检查、撤权锁竞争和末尾写入失败的完整回滚，仅发布控制元数据与排队意图。另有十项协调场景验证领取互斥、租约/派发恢复、撤权竞争和原子完成；模拟适配器回执仅是元数据证据，见 [11 持久化协调](11-reconciliation-coordination.md)。Kubernetes 协议测试覆盖响应丢失、身份/spec 冲突、条件删除与传输限额，显式真实组件测试入口见 [12 Kubernetes 适配器](12-kubernetes-adapter.md)。另在提交 `461d52c` 上通过一项显式 Kubernetes/gVisor 组件实测，[固定证据](../evidence/kubernetes-component-2026-10-09.json)记录真实虚拟机、运行时与限制。另有三项数据库和五项卷协议测试，覆盖持久化 PVC/PV 身份记录、类型筛选领取及严格 JuiceFS 回读，见 [13 卷供应](13-volume-provisioning.md)。显式 Volume worker 及独立 gVisor 文件/配额/新缓存读取探针也已通过；[组件证据](../evidence/juicefs-component-2026-10-09.json)保留 `3cf8370` 与 `30224c4` 的不同观测。Candidate 准备测试及真实 JuiceFS 命令探针覆盖独立文件、重试和目录配额，见 [14 Candidate 存储](14-candidate-storage.md)及[固定源码组件证据](../evidence/candidate-storage-2026-10-09.json)。新增 10 项 PostgreSQL 与 1 项 HTTP 场景验证独立运行 grant、激活上限、授权锁竞争和有效 scope 交集；真实 TCP 场景另执行运行授权/撤权命令，见 [15 运行授权](15-runtime-authorization.md)。运行验收 T01–T43 全部 `not_run`。现有设计检查 T00 不等于产品交付。

新增 11 项 PostgreSQL 与 1 项 HTTP 测试验证持久化启动准入、固定快照、容量竞争、取消、回滚及 WAL 恢复，见 [16 启动准入](16-start-admission.md)。

本地 Qualitygate 报告保存在已忽略的 `.qualitygate/`；提交前对最终工作区运行 `check --worktree --profile full`。报告需满足非空交付、无 pending checks、`gate.complete: true`、`gate.decision: pass` 与退出码 0，另外执行 Bazel 和静态检查。保留报告的 snapshot/policy digest，不把默认行尾检查扩大解释为功能质量保证。

运行证据字段沿用[验收记录格式](../../codespec/test/agent-computer.md)：`test_id/status/source_commit/component_digests/environment/input_refs/expected/observed/evidence_refs/limits`。当前不会生成虚假的 `passed` 运行报告。

新增 7 项 PostgreSQL 与 2 项存储测试验证已提交输入绑定、准备认领/派发、收据一致性、撤权、仅观察恢复及迁移安全，见 [17 Candidate 准备 worker](17-candidate-preparation-worker.md)。另在 `f4bff87` 上通过一项真实 worker 手动测试，覆盖实际 Volume 供应、准备命令、回执丢失后保留 inode/文件、缺失发布不重建及新挂载 S3 回读；[固定证据](../evidence/candidate-worker-2026-10-09.json)记录该组件范围，不计为完整 Computer 运行验收。

新增 10 项 PostgreSQL 和 2 项 HTTP 场景验证逻辑连接、当前权限交集、心跳 CAS、终态撤销、凭据/组织隔离及固定有效期；已有 TCP 进程测试也覆盖该生命周期，见 [18 连接会话](18-connection-sessions.md)。

新增 12 项 PostgreSQL 与 2 项 HTTP 场景验证连接所有的 Candidate 写入租约、受控派发、零派发交接、撤权及 Outbox 回滚；已有 TCP 进程场景另执行写入租约对账。准备收据为模拟元数据，不声称物理排空已完成，见 [19 写入租约](19-candidate-writer-leases.md)。

新增 7 项本地存储与 2 项 PostgreSQL 场景验证有界文件保存、未知派发及升级安全。另在 `06f3d14` 上通过真实 worker 测试：六次派发覆盖保存/重试、替换、版本冲突、写后撤权接纳、Outbox 回滚及不安全目标阻止交接；新的 JuiceFS 客户端通过 S3 读回保存文件。[固定证据](../evidence/candidate-file-save-2026-10-09.json)保留实际观测及限制，见 [20 文件保存](20-bounded-file-saves.md)。

新增两项存储、五项 PostgreSQL 与四项服务场景，覆盖有界文件读取、独立读取授权和可选 HTTP 文件网关。真实 TCP/PostgreSQL/JuiceFS/S3 组件测试在 `e7500e7` 上通过，包含 HTTP 超时后持久化完成及准确重试；[固定证据](../evidence/candidate-file-http-2026-10-10.json)记录该范围，见 [21 文件 HTTP 网关](21-file-http-gateway.md)。

新增五项监督器测试验证有界输入/输出和宿主执行拒绝。真实 gVisor 组件场景见 [22 Sandbox 监督器](22-sandbox-supervisor.md)，不授权 Candidate 写入，也不建立产品 fencing 证明。

`f7a8a2a` 上通过 13 项真实 runsc/Systrap 组件场景；[固定证据](../evidence/sandbox-supervisor-2026-10-10.json)保留精确输入与二进制摘要。监督器暂停故障明确没有本地报告，在未来可信对账前仍为未知，不计为执行成功或 fencing。

新增 11 项 PostgreSQL 与两项 HTTP 场景验证连接存续期执行预留、重试/取消、写入互斥、回滚、授权失效及迁移 12；见 [23 执行准入](23-execution-admission.md)。存储准备使用模拟证据，没有派发进程。

新增 11 项 PostgreSQL 与 1 项 HTTP 测试，覆盖执行单次派发意图、不可变恢复、派发后的取消请求/Unknown、禁止交接与迁移 13，见 [24 执行派发日志](24-execution-dispatch.md)。这些测试验证控制事务，不启动 Candidate Pod，也不证明物理排空。

新增 3 项监督器及 6 项 PostgreSQL 测试，覆盖一次性启动挑战、新鲜授权和不可变回执，见 [25 启动授权](25-execution-startup.md)。12 项显式真实 runsc 启动场景及原有 13 项监督器场景通过；测试授权帧不构成 Kubernetes/CSI 端到端执行。

启动组件测试已在[固定证据](../evidence/sandbox-startup-2026-10-10.json)中绑定 `fca66d3`，保留精确源码/二进制摘要、OCI 输入、25 项运行观测及私有 VM 回收记录。

新增 8 项协议测试与 4 项真实 K3s/gVisor 启动 attach 场景，验证有界 v5 通道，见 [26 启动通道](26-kubernetes-startup-attach.md)。夹具授权和临时工作区不构成 Candidate 执行或权威完成。

新增 7 项协议测试验证 Candidate 数据叶目录挂载及存储复核，见 [27 Candidate Pod 挂载](27-candidate-pod-mounts.md)，不构成持久执行完成。

新增 7 项 PostgreSQL 场景验证不可变执行 Pod 计划、单次创建凭据、UID 绑定启动授权、崩溃恢复、回滚和迁移 15 兼容，见 [28 Pod 身份持久化](28-execution-pod-journal.md)。模拟适配器夹具不证明实际 Pod 创建或物理排空。

新增 3 项 PostgreSQL 场景验证固定执行输入和不恢复权限的只读对账，见 [29 执行工作器](29-execution-worker.md)。工作器连接数据库派发/授权与 Candidate Pod 创建、attach，采用条件清理并保留 Unknown/Draining。真实组件运行需另行记录固定源码证据。

数据库/CSI/gVisor 执行工作器在 `bacaef9` 上通过五个真实场景，包含启动授权、运营派发命令、取消和仅观察恢复。[固定证据](../evidence/candidate-execution-worker-2026-10-10.json)包含节点 inode 检查及两次独立 S3 回读。执行仍保留 Unknown/Draining，完成接纳/排空记录为零，运行验收仍未执行。
