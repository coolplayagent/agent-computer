# 03. 架构与契约

## 03.1 实现边界

| 路径 | 职责 |
| --- | --- |
| `crates/core` | 无基础设施依赖的领域契约与状态机 |
| `crates/definitions` | 有界解析、声明语义、固定摘要和结构 Schema |
| `crates/store` | PostgreSQL 计划、声明权限、不可变 SpecVersion、原子幂等/CAS、worker 租约/回执与事件/Outbox 事务 |
| `crates/server` | Axum HTTP、请求限额、服务认证、plan/apply 与本地凭据/授权/目录运维 |
| `crates/test-support` | 仅用于集成测试的真实私有 PostgreSQL 集群 |
| `crates/cli` | 原生 CLI；提供版本、能力状态、声明验证及 Schema 导出 |
| `docs/zh-CN`、`docs/en` | 编号一致的用户与开发文档 |
| `codespec` | 详细需求、设计、决策和验收权威 |
| `knowledge` | 术语表和 CLI 管理的导航映射 |

Rust 控制服务采用 Tokio/Axum/SQLx/PostgreSQL，可信 worker 位于用户 Sandbox 外。浏览器 Driver 与 ComputerView 的适配实现沿用设计中 TypeScript/Playwright/React 边界；核心、控制面与 CLI 使用 Rust，构建统一由 Bazel 管理。

## 03.2 必须保留的不变量

1. `revision` 是元数据并发条件；`generation` 隔离旧运行实例，二者独立。
2. 身份由服务端认证产生，连接、观察、修改和 GUI 控制分别授权。
3. 租约到期不证明旧进程停止；交接要有实际排空/隔离证据，否则进入 RecoveryBlocked。
4. Unknown 不能被重试覆盖成成功或触发自动重放；业务验收归外部权威。
5. Artifact 对象先持久存储，再原子提交清单、当前指针 CAS 和 Outbox。
6. 断开连接、结束任务或停止 Computer 均不隐式删除持久 Workspace/成果，也不取消其他人的活动。

## 03.3 详细设计索引

1. [产品需求 R01–R35](../../codespec/requirements/agent-computer.md)
2. [技术设计 D01–D18](../../codespec/design/agent-computer.md)
3. [部署 DP01–DP08](../../codespec/design/deployment-automation.md)
4. [Agentic 契约 AR01–AR07](../../codespec/design/agentic-runtime-contracts.md)
5. [生态组合 E01–E10](../../codespec/design/ecosystem-integration.md)
6. [验收 T00–T43](../../codespec/test/agent-computer.md)
7. [计算与存储决策](../../codespec/decisions/compute-storage-selection.md)
8. [多 Agent 技术依据](../../codespec/decisions/multi-agent-collaboration-foundations.md)
9. [场景与缺口](../../codespec/requirements/agentic-scenarios-and-gaps.md)

新实现不能以本地内存或 mock 通过取代多节点隔离、持久化和真实运行验收。
