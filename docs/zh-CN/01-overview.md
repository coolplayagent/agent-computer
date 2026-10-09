# 01. 项目概览

agent-computer 为人和 Agent 提供可连接、可交互、可持久化的电脑基础设施。人的独立使用不依赖 Agent 或 workflow。Computer 具有稳定身份，计算实例可以替换；文件、不可变成果和检查点各有生命周期。

## 01.1 当前进度

项目进入渐进式实现阶段。已加入 Rust 工作区、Bazel 构建入口及 CLI 的 `version`、`capabilities` 命令。Computer 生命周期、租约交接、Execution/Unknown 与幂等领域规则已通过 28 项契约测试，详见 [06 领域状态契约](06-domain-contracts.md)。尚无可运行的 Computer 服务；CLI 对运行能力明确返回 `unsupported`。设计基线 0.5 是完整目标，不是当前功能列表。

## 01.2 首版范围

1. 同构单节点开发与私有多机 Linux 部署；参考 Kubernetes/containerd/gVisor 计算与 PostgreSQL/JuiceFS/S3 存储。
2. 浏览器操作、受控进程执行、文件、固定 Artifact 与应用检查点。
3. 独立链接和嵌入 ComputerView；人和 Agent 多对多连接，单 GUI 控制者，活动保护与重新观察。
4. Presentation 固定成果版本并在远程隔离环境运行生成应用。
5. 权限、撤权、幂等、fencing、未知结果、事件补读、配额和部署恢复。
6. ContextBinding、动作许可、环境交接、用途受限证据和有界准入。

完整核心目标为 [R01–R31、R33](../../codespec/requirements/agent-computer.md)。可选团队、开发和资料组合按 R34–R35 独立交付；R32 评测扩展单独验收。未启用扩展不声明支持。

## 01.3 阅读顺序

先阅读 [02 开发与构建](02-development.md)，再看 [03 架构与契约](03-architecture.md)、[04 实现计划](04-implementation-plan.md) 和 [05 验证证据](05-verification.md)。[English](../en/01-overview.md) 提供对应主题。
