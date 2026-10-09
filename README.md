# agent-computer

面向人和 Agent 的可连接、可交互、可持久化的电脑基础设施。上层产品通过链接或嵌入的 ComputerView，让用户直接进入 Computer，操作浏览器和应用，与 Agent 接续工作，并使用运行中的成果。

人可以独立使用 Computer，无需先创建 Agent 或 workflow。Computer 有稳定身份，连接与计算实例可替换；文件、成果版本和检查点按各自生命周期保留。

## 当前阶段

产品与技术设计阶段，尚无产品服务、可安装 CLI 或已验证的运行时。下列能力是首版设计目标，文档中的 API、命令和声明示例是待实现契约。

- 开发者与平台团队负责集成部署；人和 Agent 都是直接使用者。私有多机 Linux，本地采用同构单节点环境。
- 认证后的稳定连接链接与嵌入视图提供同等能力；日常 ComputerView 与运维管理界面分开。
- 浏览器优先；Chromium、结构化与视觉动作、文件界面、进程执行及生成 Web 应用的交互运行。
- 多人/Agent 可观察同一环境，每个共享 GUI 同时只有一个控制者；支持请求、交还和接续控制。
- Artifact 保存不可变成果，Presentation 将固定成果版本变为可启动、可交互的应用入口；首版不提供匿名公共托管。
- 外部 Harness 负责模型与规划，workflow-cli 负责业务流程和验收。
- 计算与存储分离；恢复文件、成果和检查点，重新建立进程。
- coolplayagent 各 CLI 独立发布，通过适配器组合。

## 文档入口

| 文档 | 内容 |
| --- | --- |
| [产品定义](codespec/requirements/agent-computer.md) | 用户、场景、边界、需求与 issue 追踪 |
| [详细技术设计](codespec/design/agent-computer.md) | 对象、接口、计算与存储、并发、恢复和部署 |
| [计算与存储选型对比](codespec/decisions/compute-storage-selection.md) | 同层候选、多维度分析、组合选择、成本、改选条件与验证方法 |
| [生态分工与集成](codespec/design/ecosystem-integration.md) | 各 CLI 的权威边界、现有能力、适配缺口和依赖顺序 |
| [验收设计](codespec/test/agent-computer.md) | 文档检查、集成测试、故障矩阵和发布证据 |
| [术语表](knowledge/glossary/business-glossary.yaml) | Computer、Sandbox、Workspace、Artifact 等术语 |

## 技术选择

控制服务采用 Rust；计算采用 Kubernetes/K3s、containerd 和 gVisor；浏览器采用 Chromium 与 Playwright；存储采用 PostgreSQL、JuiceFS 和 S3 接口对象存储。选择依据、未验证的组合和发布阻断条件见技术设计，不能从选型推断已完成兼容性测试。

选择理由见[多方案对比](codespec/decisions/compute-storage-selection.md)：已有合格 Kubernetes/S3 优先复用；已有 Ceph/HA NAS 时重新比较文件后端；需要硬件虚拟化边界时评估 Kata。新建私有对象存储优先验证 SeaweedFS 参考配置，尚未认证为可用依赖。

原始需求：[issue #1](https://github.com/coolplayagent/agent-computer/issues/1)；定位演进：[issue #2](https://github.com/coolplayagent/agent-computer/issues/2)，从成果文件扩展到过程和成品 GUI，并结合用户补充支持人直接连接与操作。研究依据：[superpod 知识库](https://github.com/stevetdp/superpod/tree/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software)。

导航地图由 relay-knowledge CLI 管理。Qualitygate 初始化策略当前只检查行尾；文档链接、示例语法与需求覆盖需另行验证，运行能力必须通过验收设计中的真实环境测试。
