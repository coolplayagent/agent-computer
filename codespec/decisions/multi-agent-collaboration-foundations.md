# 多 Agent 协作的技术依据与设计取舍

版本：设计基线 0.5；资料核对日期：2026-10-09；状态：设计参考，运行与效果实验待执行。

本文为 D18、E08–E10 提供根技术依据，不增加运行依赖。研究资料归档在 superpod 的固定提交 [`0747af8`](https://github.com/stevetdp/superpod/commit/0747af8d867b7587f02dff0c2efb1ede640ac837)：包含[根技术综述](https://github.com/stevetdp/superpod/blob/0747af8d867b7587f02dff0c2efb1ede640ac837/knowledge/software/multi-agent-collaboration-foundations.md)、[13 篇论文/报告与 6 项官方实践](https://github.com/stevetdp/superpod/blob/0747af8d867b7587f02dff0c2efb1ede640ac837/knowledge/software/multi-agent-collaboration-reading-list.md)、[机器可读证据图谱](https://github.com/stevetdp/superpod/blob/0747af8d867b7587f02dff0c2efb1ede640ac837/knowledge/software/multi-agent-collaboration-graph.json)及原件校验值。以下 C01–C09 与该图谱决策节点对应。

文献描述的机制、适用假设和实验结果，与本产品的架构选择分别记录。既有论文或官方实践不能证明本项目已经实现可靠协作，也不能证明外部组件已经具备新增接口。

## CF01. 依据与落点

| 决策 | 根技术与一手依据 | 本项目采用的约束 | 不作出的推论 | 设计 / 验收 |
| --- | --- | --- | --- | --- |
| C01 | Contract Net、AutoGen；动态任务角色与可组合会话 | Agent 与 Computer 多对多，运行位置与环境使用分别配置；纯协调/只读评审可无 Computer | 论文未规定一 Agent 一 VM，也未规定一个团队只能用一台电脑 | D18 / T38–T39 |
| C02 | SHOP2、MetaGPT、Anthropic 研究系统；任务分解和结构化产物 | 委派包含目标、固定输入、依赖、输出契约、边界及验收；业务 Task 仍由 workflow 管理 | LLM 计划不继承 HTN 的正确性保证；SOP 文本不等于持久流程 | E08–E10 / T40–T43 |
| C03 | Lamport 1978；事件因果偏序 | 消息携带所需因果父引用及输入版本；会话内游标与跨会话依赖分开 | 时间戳和全局排序不自动证明因果；不要求所有会话串行 | E08 / T40 |
| C04 | AWS 幂等 API、Outbox、Temporal Activity、RabbitMQ 确认语义 | 消息 ID 与稳定操作键关联；操作键绑定主体/请求摘要；received、processed、业务通过分开 | 本地去重或工作流持久性不保证远端副作用 exactly-once | E04、E08 / T40、T42–T43 |
| C05 | Chubby §2.4；旧持有者请求和 sequencer | 实际变更入口校验当前代次；Browser App 单控制者，旧观察/动作失效 | 只有租约超时或角色变化不足以阻止旧写入；不引入 Chubby 服务 | D18、AR02 / T39 |
| C06 | CRDT 的收敛条件、MetaGPT 的结构化交接 | 独立 Sandbox/Candidate、固定成果、显式合并和最终快照检查 | 任意代码、浏览器 profile 与共享目录不自动满足 CRDT 条件；分支通过不等于合并通过 | D18、E10-A / T39、T42 |
| C07 | Chandy–Lamport、Sagas、Temporal；一致状态与部分完成 | 查询各权威记录，明确 Unknown、对账与经授权的补偿；交接清单只承诺自身一致性范围 | HandoffManifest 不是跨 workflow/Computer/第三方 API 的全局快照；取消不撤销已发生效果 | AR03、E04、E10 / T33、T40、T42–T43 |
| C08 | Macaroons 的衰减授权、AgentDojo 的不可信工具输入 | 委派不扩大 scope，资源端重新鉴权；跨 Agent 内容不能自我授予权限或宣布业务通过 | 未选定 macaroon 协议；AgentDojo 的跨 Agent 应用是本项目威胁模型推断 | D18、E08 / T38–T40 |
| C09 | Scaling v3、MAST v3、Anthropic 简单优先实践 | 限制扇出/深度/总预算，覆盖协调错位和验收缺失，按同条件评测收益 | 不假定 Agent 越多越好，不把论文样本失效率当本产品指标 | E08 / T40；CF04 效果实验 |

可回查的原始入口包括 [Contract Net](https://www.reidgsmith.com/The_Contract_Net_Protocol_Dec-1980.pdf)、[Lamport](https://lamport.azurewebsites.net/pubs/time-clocks.pdf)、[Chubby](https://www.usenix.org/legacy/events/osdi06/tech/full_papers/burrows/burrows.pdf)、[AWS 幂等 API](https://aws.amazon.com/builders-library/making-retries-safe-with-idempotent-APIs/)、[MAST v3](https://arxiv.org/abs/2503.13657v3)和 [Scaling v3](https://arxiv.org/abs/2512.08296v3)。完整版本、关键段落、证据等级及限制保存在上述固定版本的 superpod 阅读清单中；不混用不同论文版本的实验数字。

## CF02. 环境分配与协作分组

产品默认规则是按工作与信任边界分配环境，而非固定每个 Agent 一台 Computer：

| 情况 | 选择及原因 |
| --- | --- |
| 共同观察、人与 Agent 接续同一浏览器 | 共享 Computer/Browser，单控制者串行交接；避免复制登录状态和丢失现场 |
| 并行代码或独立工具任务 | 独立 Sandbox/Candidate；可共用 Computer 逻辑身份，按预算和生命周期选择是否拆台 |
| 并行浏览器操作 | 独立 Browser App/Sandbox；同一 Browser 的不同标签页不隔离控制 |
| 私人凭据、不同信任域或网络策略 | 默认独立 Computer 与 profile/挂载，只发布获准成果 |
| 纯协调和固定成果评审 | 不强制启动环境，按需给予结果读取能力 |

Team、TaskGroup、CollaborationSession 是外部协作对象，Computer 不复制这些状态机。物理部署、Agent 进程、模型会话、Computer、Sandbox 不是同一实体；一个逻辑 Computer 也不证明所选隔离后端已经通过认证。

## CF03. 组件、仓库和部署的边界

协作基础设施与 Computer 核心独立分责。首期在 relay-teams 内形成不绑定模型 provider 的协作模块；出现第二个真实使用方、版本化接口和跨实现契约测试后再评估独立仓库。是否独立部署还需容量、故障隔离和运维证据。这是本项目降低初期集成成本的工程选择，不是论文要求。

Computer 通过授权连接、环境执行和固定成果串起其他组件：[E09–E10](../design/ecosystem-integration.md)规定配置入口与开发/资料两条流程。workflow 拥有业务 Task/Run 终态，qualitygate 提供固定快照证据，knowledge 提供版本化索引，memory 保存获准摘要；它们各自的状态不替代 Computer 执行事实。单机模式仍需验证所声明的重启恢复，拆成多个仓库也不自动获得可靠通信。

## CF04. 验证与未决问题

核心 T38–T39 用独立客户端验证多对多连接、共享控制和隔离候选；团队能力另执行 T40 的重复/迟到/乱序/撤权/崩溃测试。资料与开发组合分别执行 T43、T42。实际组件尚未实现，这些测试仍为 **not_run**。

模型效果实验由 Harness/产品团队组织：固定任务集、模型、工具版本和累计成本/token 上限，比较单 Agent、确定性 workflow、orchestrator-worker，以及有依据时的 peer 拓扑；记录多次运行的成功率、每成功任务成本、延迟分位、消息量、重复工作和验收失败。必须报告样本量和波动，不预设多 Agent 获胜。本项是产品效果研究，不将可选 AR07/T37 训练评测服务加入 Computer 核心依赖，也不拿效果分数代替故障与权限测试。

后续仍需通过实现和测量确定消息保留期、幂等记录保留期、扇出/队列阈值、任务成本模型和隔离后端组合。尚无这些实测结果，本次交付为有来源的设计基线。
