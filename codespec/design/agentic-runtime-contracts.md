# Agentic 产品场景的运行支撑契约

版本：设计基线 0.4；日期：2026-10-09；状态：新增设计，全部待实现。

本文落实 R26–R32，补充[主设计 D17](agent-computer.md)。需求来源与原有覆盖度见[场景审视](../requirements/agentic-scenarios-and-gaps.md)，验收见[测试 T31–T37](../test/agent-computer.md)。AR01–AR06 是首版核心补充；AR07 是后续可选评测扩展，未实现时 capability 必须明确为不支持。

Computer 提供环境事实、动作与执行证据。任务、消息、长期记忆、模型循环、奖励和训练分别属于集成产品、workflow、memory、Harness 与训练平台。本文件不创建第二套 Task/Run/Space 权威库。

## AR01. 产品会话到环境的显式绑定

Personal Agent 的持久电脑、团队频道的共享电脑和个人私聊的环境必须有可审计的选择规则。角色名、频道 ID、消息引用或拥有者身份都不能隐式授予使用私人文件/登录状态的权限。

新增设计对象 `ContextBinding` 由 Computer 控制面保存资源绑定；上层系统仍拥有会话和成员权威。字段为 `id/revision、organization_id、issuer、external_context_ref、mode、owner_principal_ref、computer_ref、workspace_ref、profile_policy_ref、membership_revision、expires_at`。外部引用为不含秘密的稳定 ID，不复制聊天正文。mode 为 `personal/shared/ephemeral`，与使用哪个模型或 Bot 分开。

| 模式 | 资源与隐私规则 |
| --- | --- |
| personal | 绑定经验证的个人主体；私聊不会自动接入 Bot 拥有者的电脑；不同私有主体分开实例和可写目录 |
| shared | 绑定授权共享 Computer/Workspace；只加载明确共享的连接器和 profile；成员需能访问同一画面可能披露的全部信息 |
| ephemeral | 从批准模板/固定成果创建隔离会话环境，绑定有效期；结束后按保留策略清理，成果单独保留 |

注册/修改绑定需 manage 和所引用资源的授权；可信适配器根据真实身份和成员关系解析外部会话，不信任模型提交的 owner/频道成员表。请求有效权限取主体、成员策略、绑定、资源 ACL、委托、动作许可的交集。人的独立 Computer 链接不要求 ContextBinding；绑定是产品集成入口，不能成为新增强制 Agent 依赖。

成员变更按 revision/撤销事件收敛；依赖外部成员服务的绑定必须有短时有效断言及到期拒绝规则，不能无限使用离线缓存。新请求、流投递、排队执行 dispatch 均检查当前权限；撤销停止相关输入/执行并收回受限凭据，不影响仍有权用户。已经观察或下载的内容无法远程收回。

同一 GUI 无法按观看者隐藏其中已打开的私人数据。共享电脑禁止挂载个人 profile，个人连接器结果先留在独立实例；显式审阅并提交授权成果后才可进入共享 Workspace。切换 personal/shared 创建新绑定 revision，排空旧会话；历史私人数据不因切换而自动变为团队资产。

委派采用外部身份/授权服务签发或可核验的 `delegation_ref`，记录 actor、on_behalf_of、audience、资源/动作范围、到期、撤销 revision 和父委托。子委托只能缩小权限与预算；默认不传递 profile、密钥、发布权和管理权。绑定变更不改变公共 Computer ID 的含义。

## AR02. 动作级约束与凭据代理

沿用 D10 的资源授权，补齐业务动作许可的绑定。外部业务审批服务负责批准/拒绝的业务决策，Computer 只验证和执行必要约束；有有效预授权或长期委托时直接使用，不能把每次普通操作强制变成人工审批。

对策略要求检查的操作生成 `ActionIntent`：`id、principal/delegation、binding_revision、computer/app generation、operation_kind、normalized_input_digest、target_ref、artifact_version_refs、policy_version、expires_at`。网关返回 `allow/deny/approval_required` 及机器原因；需要审批时返回关联引用，保持未执行状态，不能在等待期间持有页面控制租约阻塞其他人。

`ApprovalReceipt` 是可信决策的受控引用，绑定 intent digest、审批主体权限、资源/成果确切版本、用途、范围、有效期、撤销 revision 和使用次数。执行前再次核验实际输入、当前 ACL、代次、观察有效性与许可；审批不扩大任何已有权限。过期、参数/目标/成果变更或代次改变时拒绝旧 receipt。批准后也必须重新获取必要租约，不能沿用等待前的屏幕坐标。

许可消费与 execution 意图原子关联；并发提交只产生一个 execution，未知回执查询原 execution，不申请新许可重复副作用。外部业务结果仍可能 Unknown，动作许可和幂等记录不构成远端 exactly-once。

连接器真实凭据放在可信代理/Secret manager；沙箱获取限定 audience、目标、操作、主体和有效期的调用句柄。代理核验策略后代表主体调用批准的连接器，禁止通用 URL 转发、任意重定向带出凭据。允许域、DNS/IP、协议与响应返回范围均受模板约束，记录脱敏请求与回执。原始长期凭据不进入 prompt、环境变量、共享文件或成果。

必须明确可执行边界：一般 Shell 和浏览器点击不能可靠推断“发送/付款”等任意业务语义。声明需要业务级审批的模板必须通过可强制约束的连接器/API 路径，并阻断绕过代理的出口；做不到时拒绝无人值守能力或交人工受控操作，不能只在 prompt 中写“须审批”就声称强制安全。普通 Browser 模板的 control 授权仅是受限环境操作权，不附带未实现的业务动作识别保证。

## AR03. 环境版本与事实交接

新增 `EnvironmentVersion` 是不可变运行配置清单：Computer/App/Sandbox spec digests、镜像/工具/Skill、驱动/schema、网络/授权策略版本、存储语义配置、输入 Artifact/Checkpoint refs 和支持矩阵 ID。秘密只记录引用/版本。配置变更产生新版本，现有实例固定原版本直到按 D04/DP06 重建。

Harness 跨上下文或不同 Agent 接续时，可请求 `HandoffManifest`。它引用 `computer_id/generation、environment_version、workspace 当前 manifest 与 candidate revision、checkpoint、running/unknown execution IDs、app 状态、pending intent refs、事件 cursor、created_at`。可信控制面从权威记录生成；Harness 的计划/摘要作为明确标记的外部引用附加，不覆盖环境事实，也不包含模型私有推理。

交接不是自动 checkpoint。候选目录仍在修改、进程仍运行或跨 App 状态未形成一致恢复点时，清单必须标注 `consistency=observed`、观测时间及差异；只有 D07 已提交检查点可标记 `checkpointed`。权限裁剪导致缺失时记录 `complete=false` 和可披露原因，不把裁剪后的清单冒充完整快照。

接收者先重新鉴权、检查 generation、补读 cursor 后的事件并查询 Unknown，再获取新页面观察与控制权；发生变化则重新取事实清单。清单是恢复线索，不是 bearer token、自动权限转交或可重放指令集。多人并行仍用独立 Candidate，合并由上层依据固定成果完成。

## AR04. 执行证据与受控轨迹导出

已有 Event/Artifact 之外，增加按授权范围导出的 `ExecutionEvidenceBundle`：

| 字段组 | 权威与语义 |
| --- | --- |
| 关联 | Computer/execution/generation、binding、外部 task/run/attempt/episode/trace refs；外部 ID 只作关联 |
| 输入与环境 | EnvironmentVersion、工具/schema/策略版本、输入摘要与 Artifact refs、observation 目标/时间/坐标版本 |
| 动作与结果 | 原请求 ID、许可引用、派发/回执时间、exit/cancel/Unknown、产物 refs、错误分类和资源用量 |
| 业务验收 | 外部 verifier 的 ID/version、判定及证据引用；无验收时为 not_evaluated，不能从 exit 0 补造成功 |
| 完整性 | 事件范围/cursor、导出 schema、manifest digest、缺口/裁剪/保留到期标记、来源与权限策略 |

区分 `environment_error、policy_denied、cancelled、action_unknown、execution_failed` 与外部 `business_outcome`。Computer 不从模型陈述判定 policy_error 或生成训练奖励；策略版本、模型/Harness 元数据由对应权威提供，缺失保持 unknown。

导出默认仅元数据和受控引用；正文、截图、DOM、工具输入输出需要独立内容记录/导出策略及数据使用授权，禁止默认录制全部会话。浏览器 profile、Cookie、密码、连接凭据与模型私有推理禁止进入 Bundle。审计查看权不自动授予训练数据使用权。

创建导出和下载内容都重新检查来源 ACL 与用途；manifest 的 hash 不授予权限。来源撤权/删除发出可补读的失效事件，受控索引/训练导入器按原始 refs 撤销派生副本并返回处理记录；已导出到外部的字节不能承诺远程收回。保留策略冲突阻断导出，数据集建设方负责其复制品的授权、保留和删除。

审计回放默认只重建视图，不重新执行外部动作；报告逐项标记 recorded/live/simulated。截断/缺口存在的 Bundle 不能被标为完整训练 episode。成功任务成本由上层结合外部业务验收与 Computer 资源计量计算，保留失败、重试、等待和预热成本，不只统计成功执行。

## AR05. 长期任务唤醒、准入与资源公平

定时、消息、依赖完成、主动参与和防循环触发由外部调度器负责。Computer 提供持久事件/SSE、查询、受预算约束的 start 和执行队列；不内置 cron、通知网络或自主规划。业务等待状态由上层记录，环境可在无其他活动且完成检查点后停机。

适配器持久记录 `issuer/trigger_id/rule_version、外部 run/attempt、binding_revision、幂等键和 operation/execution IDs`；重复事件不创建第二个 Computer/执行。cursor 过期必须先快照对账，历史回放默认不唤醒任务。每次恢复重新验证当前委托、权限、预算与未知副作用；仅收到通知不构成执行许可。

准入按组织内的主体/Workspace/调用方划分有界队列；配置活跃实例上限、CPU/内存/存储配额、排队长度、deadline、累计执行预算和并发。数据库事务预留容量，实际 Kubernetes 容量不足时保留 Queued 并给出原因，不虚报 Running/Ready。持久保留和运行计算分别计量。

队列采用有界权重公平调度并防止饥饿；交互唤醒可有保留容量，后台任务按配额服务，具体权重和目标延迟由部署配置/测试固定。租约续约、取消、撤权和健康控制通道独立保留容量，不能被用户执行队列堵塞。超额拒绝返回原因与 retry_after，取消排队项释放预留；运行中抢占必须走通知、checkpoint/排空和 fencing。

预热池作为后续性能优化，按 EnvironmentVersion、架构和隔离配置分池；只保存无身份、无私人挂载/登录状态的干净实例。用过的环境默认销毁再创建；只有独立清除验证通过才能复用。预热消耗计入预算，不能从其他用户正在使用的 Computer 直接回收成空闲池。首版可不启用预热，但不能省略准入和排队状态。

## AR06. API、工具接入与版本协商

下面补充 D08 的 `/v1alpha1` 设计端点；尚无 OpenAPI 或实现。所有写入沿用幂等键、If-Match 和当前授权，长操作返回持久 operation ID。

| 接口 | 行为/限制 |
| --- | --- |
| `POST /context-bindings`、`GET/PATCH /context-bindings/{id}` | 可信集成方注册/更新绑定，逐项检查资源及成员权威；未知 issuer 拒绝 |
| `POST /action-intents`、`GET /action-intents/{id}` | 预检查动作并返回约束/许可需求；不执行副作用、不授予 GUI 控制 |
| `POST /action-intents/{id}/execute` | 绑定原输入与可核验许可，原子关联 execution；重试返回同一执行 |
| `GET /environment-versions/{id}` | 读取自身有权资源的脱敏不可变清单；配置变更由既有 plan/apply 产生新版本 |
| `POST /computers/{id}/handoffs`、`GET /handoffs/{id}` | 生成/查询按权限裁剪的事实交接清单，不自动停止/冻结环境 |
| `POST /evidence-exports`、`GET /evidence-exports/{id}` | 明确对象/时间/用途/内容策略；返回导出 operation、Bundle 和完整性限制 |
| `GET /admission/status` | 当前调用者的配额、预留和排队原因；不泄露其他主体工作内容 |

D08 的 execute/browser/File API 与 intent 接口最终进入同一授权/策略/派发路径。被策略标为需要 ActionIntent 的操作不能通过直接调用旧入口绕过；不需要额外许可的普通动作仍可直接使用原 API。事件增加 `binding.changed、intent.state_changed、environment.version_created、handoff.created、evidence.exported、evidence.access_invalidated、admission.changed`。

`/capabilities` 返回实际启用的 driver、action schema/version、环境版本、支持的资源/限制及可选扩展；发现结果按主体裁剪且不授予权限。客户端固定协商版本，未知必需能力拒绝，不能把不支持的视觉动作转成未经授权 Shell。

原生 HTTP/JSON、CLI 是核心入口。MCP 工具适配器作为独立可选交付，复用同一鉴权、请求 ID、execution 查询/取消、预算和证据，固定实际协议/schema 版本并运行契约测试。MCP 会话不是 ConnectionSession；上下文恢复不能依赖传输会话 ID。A2A Task/委派位于 Harness/workflow 适配侧；Computer 不发布虚假的 Agent Card，也不拥有 A2A 业务终态。没有 MCP/A2A 适配时，如实报告未支持。

## AR07. 可重置评测环境与 Agentic RL 扩展

这是后续 R32 扩展，T37 未通过不得宣布评测环境或训练支持。Computer 只提供环境与可验证执行记录；外部平台负责任务集、策略、rollout 调度、奖励、训练、权重同步和模型发布。

新增可选 `EvaluationEpisode`，字段包括 episode ID、EnvironmentVersion、fixture/input Artifact digest、seed、dataset/split refs、允许副作用、verifier ref、budget、generation、状态和证据。训练/保留评测集分配由外部数据治理服务核验；Computer 记录引用和限制，不自行保证全局无数据污染。

| 环境操作 | 契约 |
| --- | --- |
| create | 从固定版本与 fixture 创建一次性 Computer，标记 purpose=evaluation；无生产成员、profile 或凭据 |
| reset | 停止并 fence 旧执行，创建新 generation 和独立可写状态，恢复固定 fixture；旧句柄/动作不可继续写 |
| step | 复用受控动作与回执；请求含 episode/generation/step ID，重复步骤查询原结果，返回 observation 与执行终态 |
| verify | 由隔离的 verifier 读取固定结果/权威模拟服务状态；验证器和标准不可被被测 Agent 修改 |
| close | 排空并清理一次性环境；按授权保留证据，失败与未完成均有准确终态 |

默认网络只允许批准的测试站点/模拟器，禁止生产账户和真实不可逆交易。重置只覆盖声明内 fixture/本地状态，不能撤销真实网站副作用；需要外部系统重置时必须有该系统的测试 adapter 与独立证据，否则标记 non_resettable 并拒绝相应评测承诺。

相同 seed 不保证真实网页、模型输出、时钟或网络确定性。报告声明可重现范围、固定版本、外部变量、失败类别和样本量；环境故障/权限拒绝不能自动算作策略负奖励。Episode 证据通过 AR04 授权导出，生产轨迹不得自动进入训练。未来若增加低成本 fork、内存快照、GPU 或原生桌面，应分别增加能力协商、隔离与恢复验收，不能从本接口推定已支持。
