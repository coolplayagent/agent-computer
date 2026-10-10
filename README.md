# agent-computer

面向人和 Agent 的可连接、可交互、可持久化的电脑基础设施。上层产品通过链接或嵌入的 ComputerView，让用户直接进入 Computer，操作浏览器和应用，与 Agent 接续工作，并使用运行中的成果。

人可以独立使用 Computer，无需先创建 Agent 或 workflow。Computer 有稳定身份，连接与计算实例可替换；文件、成果版本和检查点按各自生命周期保留。

agent-computer 为人和 Agent 团队提供可共享、可隔离、可接续的电脑环境，并以环境执行、固定成果和适配器串起生态组件。Agent 按任务与权限分组，Computer 按环境与隔离需求分配，两者通过授权连接形成多对多关系；Agent 的模型循环运行位置与其使用的 Computer 分开。

relay-teams/Harness 负责团队协作与模型执行，workflow 负责持久业务流程与验收，Computer 负责环境、动作和成果事实。通用协作通信首期在 relay-teams 内形成独立模块，待第二个真实复用方和稳定契约出现后再评估独立仓库；Computer 的独立使用不依赖该模块。

## 当前阶段

渐进式实现阶段，完整目标保留设计基线 0.5。已建立 Rust + Bazel 工作区、声明验证 CLI、PostgreSQL 持久化与原子 plan/apply、Axum 控制服务、服务凭据和资源运行授权。协调、启动准入、连接会话与 Candidate 修改租约已实现；JuiceFS 卷供应、Candidate 准备、有界文件保存及文件 HTTP 网关已通过对应组件测试。

执行链路已连接单次数据库派发、Candidate Pod 创建和 CSI 身份复核、启动挑战与有界 gVisor attach。[31 节点布防后的启动](docs/zh-CN/31-node-guarded-startup.md)新增本地 Pod/运行时/cgroup/Candidate 身份绑定，要求独立节点 watchdog 布防并持久登记后才能发出启动授权。[40 可撤销 Candidate 文件系统](docs/zh-CN/40-candidate-io-fence.md)新增可封存的 FUSE 修改入口与挂载内 I/O 排空屏障；[41 执行 CSI 集成](docs/zh-CN/41-fenced-execution-csi.md)已把它接入实际 Pod，并绑定节点身份、首次 Pod UID 和不可逆发布撤销。[42 执行完成](docs/zh-CN/42-accepted-execution-completion.md)把活的进程与 I/O 封闭证明、持久输出及原子写租约释放接入可信 worker；缺少证明或控制器丢失时仍保留 Unknown/Draining，跨节点 fencing 和自动排空恢复尚未交付。[43 执行输出下载](docs/zh-CN/43-execution-output-downloads.md)新增经过当前授权和对象完整性复核的 stdout/stderr HTTP 下载。[44 常驻执行队列 worker](docs/zh-CN/44-queued-execution-worker.md)新增精确组织与存储身份的原子领取、有界并发和停止后等待已领取任务完成；重启不会重放已派发请求。

当前源码通过 393 项默认 Cargo 测试及 14 个 Bazel 测试目标，另有固定源码的 K3s/gVisor、JuiceFS/PostgreSQL/S3 组件证据。宿主子进程回收修复后的七个节点布防启动场景已完成 VM 复验，证据已回读并清理临时 VM。组件通过不表示完整 Computer 可用：纯文件 Artifact/checkpoint、S3 恢复、同 Workspace 并行 Candidate 与分支继续编辑已交付；浏览器、ComputerView、完整 App checkpoint、Presentation、部署运维及生态适配仍待实现，公开执行能力仍不支持，T01–T43 保持 `not_run`。构建方法与完整进度见[编号中英文文档](docs/README.md)。以下仍为首版目标，CodeSpec 中未交付的 API、命令和声明属于待实现契约。

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
