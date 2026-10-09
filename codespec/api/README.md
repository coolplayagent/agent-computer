# 公共接口

首版 API、CLI、事件与声明契约集中在[技术设计 D03、D08、D14、D15](../design/agent-computer.md)，包括人的独立连接、嵌入和运行成果。它们是待实现接口，当前尚无可执行 OpenAPI/JSON Schema 或产品二进制。

后续从设计导出机器协议，并以真实适配测试验证兼容；不能将设计示例当作已可调用服务。

[Agentic 契约 AR06](../design/agentic-runtime-contracts.md)补充绑定、许可、交接、证据与准入接口，以及可选 MCP 适配的责任边界；AR07 的评测操作属于后续可选扩展。[部署契约 DP03/DP04](../design/deployment-automation.md)定义独立安装声明和拟交付部署命令，两者均未实现。

[多 Agent 设计 D18](../design/agent-computer.md)复用连接、绑定和成果接口，不新增 Computer Team/TaskGroup/Message 权威对象。[生态集成 E08–E10](../design/ecosystem-integration.md)定义外部协作通信责任、集成宿主读取的组合配置及端到端契约；该配置不属于 ComputerSet，也不是已有可执行 CLI/schema。
