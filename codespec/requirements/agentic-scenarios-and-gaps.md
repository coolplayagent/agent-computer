# Agentic 演进、产品场景与能力缺口

版本：设计基线 0.4；审视日期：2026-10-09；状态：研究与设计补充，未实现或运行验收。

本文以 agent-computer 修改前提交 `a24a54525632703f3650496c9f8ba18660d1acbe` 为覆盖基线，使用 superpod 固定提交 `54f75c3f487dbcfe1f47779d0903383d5bce0ada` 的知识资料，并复核相关公开工程来源。它补充[产品需求](agent-computer.md)、[Agentic 运行契约](../design/agentic-runtime-contracts.md)和[部署契约](../design/deployment-automation.md)。

## SC01. 证据范围与状态口径

当前仓库只有产品、设计与验收文档，没有产品服务、可安装 CLI、Chart、运行时或实测报告。因此下文“已有”仅指原有设计覆盖；所有产品运行能力均为未实现/未验证，不能以补完文档把缺口改成“功能已具备”。

superpod 是研究知识库，其产品资料包含厂商声明、归档原文、参考架构和未验证分析。本文把产品名称用作场景线索，不声明已试用产品、掌握其内部实现或复现论文结果。Agent Space 指资源/协作空间技术；AI-IM 指 AI 原生即时通讯；Agentic RL 指环境交互驱动的策略训练，三者不混作品牌或同义词。

| 来源 | 固定知识入口 | 本次使用范围 |
| --- | --- | --- |
| S01 | [Harness 与执行闭环](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agentic-ai-harness-and-loop.md) | 环境、会话、模型循环分离；长任务恢复、停止与副作用 |
| S02 | [产品定义与架构](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/ai-agent-products-2026.md)、[Personal Agent 原始定义](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/personal-agent-and-ai-im-definitions.md) | Muse/Cue 的持续职责、Team Bots 的会话/电脑边界、Manus 的项目与成果场景；仅沿用知识库的来源限定 |
| S03 | [AI 原生即时通讯](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/ai-native-instant-messaging.md) | 频道/私聊、委托、接续、任务与成果审阅；消息不等于执行授权 |
| S04 | [Agent Space 核心技术](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-space-core-technologies.md)、[技术图谱](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-product-technology-map.md) | 身份、资源、事件、版本、证据和训练入口的参考契约 |
| S05 | [多 Agent 协作](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/multi-agent-teams-and-coordination.md)、[任务与订阅](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-task-system-and-subscriptions.md) | 委派、单写/分支、事件唤醒、取消、预算、公平和防重复 |
| S06 | [Context 与 Memory](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-context-memory-and-state.md)、[互操作](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-interoperability-protocols.md) | 权威状态与派生记忆分离、委托链、协议发现不授予权限；不直接采用未认证协议版本 |
| S07 | [Agent 评测](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-evaluation-and-reliability.md)、[Agentic RL](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agentic-rl-training-and-productization.md) | 固定环境、reset/step/verify、轨迹、故障分类、训练与保留评测隔离 |
| S08 | [持久存储与成本](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-persistent-storage-and-economics.md)、[Computer](https://github.com/stevetdp/superpod/blob/54f75c3f487dbcfe1f47779d0903383d5bce0ada/knowledge/software/agent-computer-browser-and-gui.md) | 持久身份/文件与短期计算分离，GUI 与 API 组合、成功任务成本 |

本次本地读取以上原文；superpod 的 `knowledge/software` 图索引覆盖 40 个文件、固定提交且无内容降级。查询返回有数量截断，引用均通过直接读取核对，不以检索摘要宣称全库穷尽。agent-computer 基线图有 6 个 text-only 文件（忽略文件、LICENSE、旧地图），设计仍以原文为准。

外部交叉核对：[Anthropic Managed Agents](https://www.anthropic.com/engineering/managed-agents)支持会话/Harness/执行环境可独立替换的工程方向；[长任务 Harness](https://www.anthropic.com/engineering/effective-harnesses-for-long-running-agents)支持跨会话以可检查成果接续；[Agent 评测实践](https://www.anthropic.com/engineering/demystifying-evals-for-ai-agents)支持轨迹与实际结果分开验证；[AgentRL 固定提交](https://github.com/THUDM/AgentRL/tree/6a73409d31ba695d383b978a8ad3ef400d90c054)支持训练组件与任务环境分离。这些不是本项目性能或安全证明。

## SC02. 演进方向及对底座的要求

以下是从 S01–S08 归纳的并行演进方向，不是所有产品依次经历的行业时间线，也不按模型品牌排名。

| 方向 | 产品体验变化 | Computer 底座受到的要求 |
| --- | --- | --- |
| 单轮回答/工具调用 → 长时可恢复工作 | 用户离开后继续，跨上下文/模型/worker 接续 | 稳定资源 ID、持久执行回执、事实交接、Unknown 对账；环境存活不依赖模型进程 |
| 单人会话 → 个人代理与团队协作 | 同一能力用于私人事务、共享岗位和频道任务 | 显式环境归属、成员撤权、个人/共享凭据与画面隔离、逐跳委托 |
| 文本答案 → 可编辑与可运行成果 | 用户接管过程，继续编辑并使用生成应用 | ComputerView、Artifact 版本、Presentation、人的活动保护和独立验收 |
| 前台请求 → 定时/事件触发的持续职责 | 等待输入或事件后恢复，多个任务竞争环境 | 可补读事件、稳定触发键、启动准入、配额、取消和公平调度；外部拥有触发规则 |
| 单次执行 → 可评测、可学习系统 | 对比模型/Harness，收集授权轨迹并离线训练 | 环境版本、可导出证据、可重置 fixture、独立 verifier、数据使用权限 |

这些方向支持继续把 agent-computer 定位为“人与 Agent 共用的持久环境和执行基础设施”。环境、模型循环与任务权威保持分离，便于更换 Harness，并允许一个 Harness 操作多个环境、一个环境被不同主体依次接续。无需把聊天、模型推理或训练引擎迁入 Computer。

## SC03. 按支撑产品场景检查能力

“完整”仅指 0.3 文档中已有主要契约；“部分”表示有基础对象但缺少产品闭环；“缺失”表示原设计未定义。表中“本次补充”也全部是待实现规格。实现状态对全部行统一为未实现，外部产品能力不能计入本仓库实现。

| 场景 | 基线设计覆盖与证据 | 关键缺口 | 本次补充/责任与验收 |
| --- | --- | --- | --- |
| C01 人独立使用电脑、Agent 接续 | 完整：稳定 Computer、连接、Browser/File UI、单控制者；D02/D09/D14 | 没有运行实现；新增交接事实包可减少各 Harness 自行拼接 | 保持 R23/R24；R28/AR03 补齐交接，T21–T24/T33 |
| C02 持续个人代理：私人研究、办公、账户操作 | 部分：持久文件/profile、恢复、主体 ACL；D05/D10/D11 | 私聊选哪台电脑、代表谁执行、凭据代理和动作许可不够具体 | R26/R27，AR01/AR02，T31/T32；个人记忆与职责仍由上层拥有，来源 S02/S06 |
| C03 AI-IM/团队 Bot：频道委托和个人连接器 | 部分：多人观察、共享 Workspace 与成果；D06/D14 | 频道与私人实例绑定、成员变化、私人结果分享规则 | ContextBinding 与独立私人执行；R26，T31。消息、频道和任务仍属宿主，来源 S02/S03 |
| C04 多 Agent 研究/编码、交班与审阅 | 部分：独立 Candidate、CAS、固定 Artifact；D06/D07/E04 | 结构化 handoff、子委托权限与累计资源治理 | R26/R28/R30，AR01/AR03/AR05，T31/T33/T35；分解/合并决策属 Harness/workflow，来源 S05 |
| C05 定时任务、订阅事件、等待人工后恢复 | 部分：SSE/Outbox、幂等、idle/checkpoint；D04/D08/E04 | 触发桥接去重、重放不重执、启动队列与公平预算 | R30/AR05，T35；cron/通知/主动参与/防循环触发在外部调度器，来源 S01/S05 |
| C06 项目创作和生成 Web 应用交付 | 完整：固定 Artifact + Presentation、隔离试用、保存新版本；D15 | 已有设计未运行；审阅/批准与固定版本关联需贯通 | 保持 R20/R21；R27/R29 补许可和证据，T25–T27/T32/T34，来源 S02/S04 |
| C07 企业跨应用事务和敏感操作 | 部分：ACL、Secret 引用、默认拒绝网络；D10 | 权限逐跳缩小、审批输入绑定、代理执行与旁路范围 | R27/AR02，T32；通用浏览器不声称识别所有业务副作用，来源 S01/S02/S06 |
| C08 多 Harness 和协议接入 | 部分：HTTP/CLI、capabilities、workflow adapter；D08/E02 | 能力/schema 固定、传输会话与业务句柄区分、MCP 可选适配契约 | R31/AR06，T36；A2A 任务属于 Harness/workflow；协议适配仍未实现，来源 S06 |
| C09 记忆与知识积累 | 部分：固定成果/事件来源、memory/knowledge 外部边界；E02 | 导出用途授权、来源撤权与派生数据失效联动 | R29/AR04，T34；只提供受控事实来源，不实现记忆检索/上下文压缩，来源 S06 |
| C10 平台私有云交付、弹性与运维 | 部分：多节点选型、预算、备份原则；D12/CS09 | 可重复安装、状态归属、升级兼容窗口与整体验收 | R25/DP01–DP08、R30/AR05，T28–T30/T35；尚无可安装发行件 |
| C11 Agent 产品回归与离线 Agentic RL | 缺失：只有运行验收矩阵，无对外 episode/reset 或轨迹导出 | 版本化环境、故障分类、用途授权、重置与独立 verifier | R28/R29 为核心；R32/AR07/T37 是后续评测扩展；训练/奖励/权重更新属外部平台，来源 S07 |
| C12 原生桌面、多模态与快速内存接续 | 缺失：首版明确 Browser/WebApplication 与文件恢复；R14/D04 | Desktop Driver、音视频、GPU/设备、内存快照未设计认证 | 保留后续路线，不能从 Chromium GUI 推定完整桌面或 VM 快照，来源 S08 |

已有设计最完整的是“可连接电脑 + 共享工作/固定成果 + 人机交接 + 可运行成品”。主要补齐方向为环境归属与授权、跨会话事实、证据和资源治理。构建个人助手、AI-IM 或训练服务还需要表中明确的外部能力，不能直接把 Computer 包装为这些完整产品。

## SC04. 优先级与明确保留的缺口

| 顺序 | 本项目工作 | 完成依据 |
| --- | --- | --- |
| P0 基础可用性 | 先验证 gVisor/Browser/存储路径和已有 D1 实验，再实现 0.3 核心 | T01–T27 与 CS09；不能因新增功能跳过原有隔离/持久化门 |
| P0 首版产品接入 | R25–R31：部署、绑定/委托、动作约束、环境交接、证据、准入和原生能力协商 | T28–T36；可选 MCP 未启用时明确 unsupported，启用则必须通过对应契约项 |
| P1 评测与性能扩展 | R32/AR07 的一次性评测环境；基于容量证据再增加预热/fork 优化 | T37；相关能力单独发布，默认不接生产账号、不自动导出训练数据 |
| 后续条件化扩展 | Desktop/音视频、GPU、内存快照、其他运行时/存储认证、多地域 | 独立需求与兼容/故障测试；暂无产品支持承诺 |
| 外部长期责任 | 模型/Harness、消息与通知、Task/SOP、长期记忆、策略训练、模型发布 | 明确适配责任与端到端集成测试；不以外部项目存在认定集成完成 |

R25–R31 的本次完成物是可评审设计，不是可用功能。后续应从“团队在共享频道发起研究，个人完成受控登录，Agent 接续生成应用，人独立使用成果”这一闭环开始实现和验收；同时验证私人信息不进入共享画面、重复消息不重复执行、取消不删除成果、旧审批不批准新版本。再在隔离 fixture 上验证同一任务的回归与证据导出。

衡量指标包括首帧/启动/恢复、任务独立验收、人工介入、Unknown/权限拒绝、排队与饥饿、资源总成本及故障分类。部署吞吐、成功请求数、模型自报完成或训练 GPU 利用率均不能代替业务成果验收。具体目标应在基准前冻结；本轮不填写未经测量的性能或 SLA。
