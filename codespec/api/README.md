# 公共接口

首版 API、CLI、事件与声明契约集中在[技术设计 D03、D08、D14、D15](../design/agent-computer.md)，包括人的独立连接、嵌入和运行成果。当前已交付 Rust CLI 的 `version/capabilities/validate/schema`，以及 [ComputerSet JSON Schema](../../schemas/computer-set-v1alpha1.json)；用法与语义边界见 [docs 07](../../docs/zh-CN/07-declarations.md)。内部 PostgreSQL 声明库及事务边界见 [docs 08](../../docs/zh-CN/08-persistence.md)；它不对外提供数据库端点，不执行资源 apply。现已交付 [HTTP/OpenAPI 子集](../../schemas/openapi-v1alpha1.json) 与可撤销服务凭据，启动/路由/限制见 [docs 09](../../docs/zh-CN/09-control-service.md)。现已提供具有声明/引用授权的 plan/apply、计划/operation 查询及不可变 SpecVersion 发布，详见 [docs 10](../../docs/zh-CN/10-plans-and-apply.md)。OIDC、运行权限和 Computer 协调器尚未实现，新 operation 保持 Queued；已提供协调进度/水位和可信租约/回执接口，见 [docs 11](../../docs/zh-CN/11-reconciliation-coordination.md)。

ComputerSet Schema 验证结构，Rust 验证器补充静态引用、预算和 driver 约束。后续运行协议仍需真实适配测试验证兼容；不能将声明验证成功当作已可调用服务。

[Agentic 契约 AR06](../design/agentic-runtime-contracts.md)补充绑定、许可、交接、证据与准入接口，以及可选 MCP 适配的责任边界；AR07 的评测操作属于后续可选扩展。[部署契约 DP03/DP04](../design/deployment-automation.md)定义独立安装声明和拟交付部署命令，两者均未实现。

[多 Agent 设计 D18](../design/agent-computer.md)复用连接、绑定和成果接口，不新增 Computer Team/TaskGroup/Message 权威对象。[生态集成 E08–E10](../design/ecosystem-integration.md)定义外部协作通信责任、集成宿主读取的组合配置及端到端契约；该配置不属于 ComputerSet，也不是已有可执行 CLI/schema。

已交付[认证执行 CLI](../../docs/zh-CN/52-authenticated-execution-cli.md)，接入已有运行 HTTP 子集；与 D08 中尚未实现的 Browser、App、Presentation 和事件命令分开。
