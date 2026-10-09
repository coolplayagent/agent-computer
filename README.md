# agent-computer

面向人和 Agent 的可连接、可交互、可持久化的电脑基础设施。上层产品通过链接或嵌入的 ComputerView，让用户直接进入 Computer，操作浏览器和应用，与 Agent 接续工作，并使用运行中的成果。

人可以独立使用 Computer，无需先创建 Agent 或 workflow。Computer 有稳定身份，连接与计算实例可替换；文件、成果版本和检查点按各自生命周期保留。

agent-computer 为人和 Agent 团队提供可共享、可隔离、可接续的电脑环境，并以环境执行、固定成果和适配器串起生态组件。Agent 按任务与权限分组，Computer 按环境与隔离需求分配，两者通过授权连接形成多对多关系；Agent 的模型循环运行位置与其使用的 Computer 分开。

relay-teams/Harness 负责团队协作与模型执行，workflow 负责持久业务流程与验收，Computer 负责环境、动作和成果事实。通用协作通信首期在 relay-teams 内形成独立模块，待第二个真实复用方和稳定契约出现后再评估独立仓库；Computer 的独立使用不依赖该模块。

## 当前阶段

渐进式实现阶段，完整目标保留设计基线 0.5。已建立 Rust + Bazel 工作区、`version/capabilities/validate/schema` CLI 及 Computer/Lease/Execution/幂等领域规则（28 项领域测试）；ComputerSet YAML/JSON 静态验证、固定摘要与 JSON Schema 已交付，另已交付 PostgreSQL 声明库、原子幂等/CAS 与事件/Outbox，新增 Axum 控制服务、可撤销服务凭据、声明权限与 plan/apply API，可原子发布不可变资源版本和协调意图；另已实现协调租约、派发不确定性/回执和进度查询，并新增受限 Kubernetes HTTPS 适配器与 JuiceFS 卷供应 worker，新增 Candidate 独立文件/配额准备器，并新增独立运行授权、受限权限查询与持久化启动排队/容量预留/取消，新增固定 Workspace 输入与 Candidate 准备 worker，并已实现凭据绑定的连接会话、心跳与断开，以及连接所有的 Candidate 修改租约、续租和安全交接元数据，另已接入有界文件保存、内容版本检查及完成后的排空证明，共 213 项默认测试（88 数据库、13 服务、18 Kubernetes、31 存储、63 原有测试）。适配器支持无 Workspace 挂载的临时 Pod 创建、身份核对与条件删除；卷 worker 可认领授权意图、供应保留型 PVC 并持久记录 PVC/PV UID，其余资源意图仍待后端协调。单虚拟机中的 K3s/gVisor 与 JuiceFS/PostgreSQL/S3 组件测试已通过，包含文件 fsync、重建读取与配额拒绝探针，Candidate 准备器亦通过真实文件、重试与配额探针，启动准入已分配持久化 generation/Candidate 身份，新 Workspace 的空初始输入绑定及存储准备派发已实现，Candidate worker 另通过真实 CSI/数据库/存储恢复测试（见 [17 准备 worker](docs/zh-CN/17-candidate-preparation-worker.md)），非空 Artifact 输入、Pod 挂载和完整 Computer 运行时尚待实现。构建方法与各阶段进度见 [编号中英文文档](docs/README.md)。下列能力仍是首版设计目标，CodeSpec 中的 API、业务命令和声明示例是待实现契约。

- 开发者与平台团队负责集成部署；人和 Agent 都是直接使用者。私有多机 Linux，本地采用同构单节点环境。
- 认证后的稳定连接链接与嵌入视图提供同等能力；日常 ComputerView 与运维管理界面分开。
- 浏览器优先；Chromium、结构化与视觉动作、文件界面、进程执行及生成 Web 应用的交互运行。
- 多人/Agent 可观察同一环境，每个共享 GUI 同时只有一个控制者；支持请求、交还和接续控制。
- Artifact 保存不可变成果，Presentation 将固定成果版本变为可启动、可交互的应用入口；首版不提供匿名公共托管。
- 外部 Harness 负责模型与规划，workflow-cli 负责业务流程和验收。
- 计算与存储分离；恢复文件、成果和检查点，重新建立进程。
- 私人/共享会话显式绑定环境；动作许可、交接事实、证据导出和资源准入共同支撑长任务与团队产品。
- coolplayagent 各 CLI 独立发布，通过适配器组合。
- 提供独立电脑、Agent 团队、持久流程、开发/资料处理及知识接续的组合方案；配置、适配契约和端到端示例见生态集成 E08–E10，均待实现。

## 文档入口

按语言与序号阅读：[01 中文](docs/zh-CN/01-overview.md) · [02 English](docs/en/01-overview.md) · [完整目录 / Contents](docs/README.md)。

| 文档 | 内容 |
| --- | --- |
| [产品定义](codespec/requirements/agent-computer.md) | 用户、场景、边界、需求与 issue 追踪 |
| [详细技术设计](codespec/design/agent-computer.md) | 对象、接口、计算与存储、并发、恢复和部署 |
| [计算与存储选型对比](codespec/decisions/compute-storage-selection.md) | 候选比较、gVisor/JuiceFS 成熟度与采用条件、成本和验证方法 |
| [多 Agent 协作技术依据](codespec/decisions/multi-agent-collaboration-foundations.md) | 根技术图谱、论文/实践、产品边界与设计映射 |
| [部署与运维契约](codespec/design/deployment-automation.md) | 已有/托管集群优先、Terraform/Helm 分工、安装、升级、恢复与认证 |
| [Agentic 场景与缺口](codespec/requirements/agentic-scenarios-and-gaps.md) | superpod 固定证据、12 类产品场景、已有设计/缺口和优先级 |
| [Agentic 运行契约](codespec/design/agentic-runtime-contracts.md) | 环境绑定、动作许可、交接、证据、准入、协议和可选评测扩展 |
| [生态分工与集成](codespec/design/ecosystem-integration.md) | 组件权威边界、协作通信归属、组合配置、两条端到端流程与依赖顺序 |
| [验收设计](codespec/test/agent-computer.md) | 文档检查、集成测试、故障矩阵和发布证据 |
| [术语表](knowledge/glossary/business-glossary.yaml) | Computer、Sandbox、Workspace、Artifact 等术语 |

## 技术选择

待认证参考组合为 Rust 控制服务、Kubernetes/containerd/gVisor、Chromium/Playwright，以及 PostgreSQL、JuiceFS 和 S3 接口对象存储。已有合格或托管 Kubernetes 优先，K3s 是新建私有基础设施的参考路径。选择依据、未验证的组合和发布阻断条件见技术设计，不能从选型推断已完成兼容性测试。

选择理由见[多方案对比与成熟度分析](codespec/decisions/compute-storage-selection.md)：gVisor 提供额外隔离边界，JuiceFS 支撑跨节点共享工作目录；二者有上游生产使用依据，但本项目的浏览器、CSI、元数据库及恢复组合仍待测试。已有 Ceph/HA NAS 时重新比较文件后端；需要硬件虚拟化边界时评估 Kata。新建私有对象存储优先验证 SeaweedFS 参考配置，尚未认证为可用依赖。

原始需求：[issue #1](https://github.com/coolplayagent/agent-computer/issues/1)；定位演进：[issue #2](https://github.com/coolplayagent/agent-computer/issues/2)，从成果文件扩展到过程和成品 GUI，并结合用户补充支持人直接连接与操作。研究依据：[superpod 知识库](https://github.com/stevetdp/superpod/tree/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software)。

导航地图由 relay-knowledge CLI 管理。Qualitygate 初始化策略当前只检查行尾；文档链接、示例语法与需求覆盖需另行验证，运行能力必须通过验收设计中的真实环境测试。
