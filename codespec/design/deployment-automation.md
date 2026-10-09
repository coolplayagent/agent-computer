# 云服务自动化部署与运维契约

版本：设计基线 0.4；日期：2026-10-09；状态：待实现，尚无安装器、Chart、IaC 模块或部署认证。

本文落实 R25，补充[主设计 D12、D16](agent-computer.md)。选型依据见[CS01–CS10](../decisions/compute-storage-selection.md)，验收见[测试 T28–T30](../test/agent-computer.md)。本文命令和文件名是拟交付接口，不是现有安装说明。

## DP01. 交付目标与支持范围

平台管理员提供版本化环境配置后，可以预览资源变化、安装、验证、升级、扩缩容、诊断和恢复。安装成功必须有 Computer 端到端使用证据，不能只依据 Helm release 或 Pod Ready。部署在组织自己的云账号/VPC 或私有 Linux 环境中；公共多租户 SaaS、计费平台和跨地域双活仍在首版范围外。

| 路径 | 交付件 | 前提与责任 |
| --- | --- | --- |
| 已有合格 Kubernetes | 官方应用 Helm Chart、values schema、预检与验收程序 | 优先复用已有或托管平台；管理员提供支持 runsc、可信 CSI、网络策略和持久服务的节点池 |
| 新建私有集群/云主机 | 一个首批认证云环境的 Terraform 模块、节点镜像/引导配置、同一 Chart | 组织负责云账号、配额和基础设施运维；K3s 是自建参考路径，不能据有云账号推定必须自建 |
| 本地 Linux amd64 | 单节点 K3s 配置、同一应用镜像与核心存储路径 | 同构开发与试用；单机数据库/对象服务不具有生产 HA 保证 |
| 离线环境 | 镜像/二进制/Chart/依赖离线包、导入与签名校验 | 以上路径的交付变体；完整断网验收后才列为支持，首批在线认证不代表离线认证 |

优先选择满足运行时、存储确认语义和数据位置要求的现有/托管依赖。新建 K3s HA 参考配置为 3 个 server 的 embedded etcd 和独立执行节点池；分别验证控制面 quorum、数据库同步确认、对象冗余、入口和节点 fencing。故障域与网络延迟须匹配数据库/etcd 要求，多台机器本身不证明 HA。[K3s HA](https://docs.k3s.io/datastore/ha-embedded)

核心应用 Chart 不自动安装另一套数据库或对象集群。试用依赖、生产依赖参考配置与应用分别发布和升级。已有合格 S3/NAS/Ceph 的复用须遵循 CS04/CS05；JuiceFS 是共享工作目录参考后端，替代后端未经适配和同等验收不得列为已支持。

## DP02. 工具分层与唯一管理者

| 层 | 管理对象 | 执行工具/权威 |
| --- | --- | --- |
| 云资源 | 网络、安全组、LB、节点池、托管依赖 | Terraform 状态与云 API；外部现有资源显式引用或经审阅导入 |
| 节点 | OS、containerd/runsc、K3s、缓存盘、节点标签 | 固定镜像和 cloud-init；已有主机可使用版本化 Ansible 配置 |
| 平台应用 | API、协调器、网关、ComputerView、RBAC、探针和观测接入 | Helm release 或选定的 GitOps 控制器；每个资源只有一个期望状态管理者 |
| Computer 运行资源 | 按已授权定义创建的 Sandbox/App/连接与租约 | Computer 协调器；GitOps 不逐个接管临时执行 Pod |
| 产品部署入口 | 参数检查、生成配置、分阶段执行、状态与诊断 | `agent-computer deploy` 薄适配层；不复制 Terraform 的资源数据库或引入通用工作流引擎 |

安装器生成标准 Terraform inputs、Helm values 和执行清单，支持分别执行并由平台 CI/CD 接管。GitOps 模式通过版本化声明变更，不再由 CLI/另一 Helm 流程同时修改同一资源。紧急运维变更应回写声明或显式解除管理权，不能长期留下双写。[OpenGitOps](https://opengitops.dev/)

基础设施、依赖和应用发布采用分开的状态与变更计划，通过固定 outputs/引用连接。Terraform 不以大量 `remote-exec` 脚本管理所有后续行为；节点引导使用镜像或专用配置工具，安装器单独验证引导就绪。[Terraform provisioners](https://developer.hashicorp.com/terraform/language/provisioners)、[模块组合](https://developer.hashicorp.com/terraform/language/modules/develop/composition)

## DP03. 声明与状态

拟定入口文件 `deployment.yaml` 保存安装意图，与创建业务 Computer 的 `ComputerSet` 分开。完整 JSON Schema、生成器与参数映射尚待实现；未知字段、冲突的资源管理者及不支持的组合必须拒绝。schema/默认值变更随 release 版本化，不静默改变已有环境。

| 配置组 | 必需内容 |
| --- | --- |
| identity/release | 稳定 installation ID、环境、配置 revision、release lock 引用 |
| infrastructure | existing/provision 模式、云模块与版本、资源引用、远端 state backend 引用 |
| cluster/nodePools | kube context/集群身份、节点角色、RuntimeClass、容量上下限、故障域 |
| dependencies | 控制库与文件元数据库的独立库/角色引用、S3、Workspace backend 与能力声明 |
| access | 域名、TLS、OIDC、嵌入 origin、WSS/SSE 路由与超时、私网/出口策略 |
| operations | 配额、并发/排队预算、观测 endpoint、备份保留、维护和数据回收策略 |
| secrets | Secret manager/工作负载身份引用；声明不保存密码、Cookie 或长期云密钥 |

优先继承标准工具的 state、锁与状态查询；部署执行清单只记录 installation ID、配置/计划摘要、操作 ID、阶段结果、资源引用和证据，不成为另一份资源权威。状态保存在受控持久位置，CLI 退出或换机器后可继续查询；不能仅存在待部署/待恢复的集群内。

Terraform 使用有访问控制、加密、备份和锁定能力的远端 backend。state 可能包含敏感值，标记 sensitive 不等于秘密不存在；限制读取，避免秘密写入计划日志。基础状态、解密密钥和恢复资料必须在目标集群损坏后仍可取得。[远端状态](https://developer.hashicorp.com/terraform/language/state/remote)

## DP04. 安装与中断恢复

拟交付命令：

```text
agent-computer deploy plan -f deployment.yaml --json
agent-computer deploy apply --plan <plan-id> --json
agent-computer deploy status <operation-id> --json
agent-computer deploy verify --installation <installation-id> --json
```

1. **plan**：校验静态配置、依赖与权限，输出资源归属、增删改、费用相关资源量、停机影响、迁移步骤、数据删除范围及缺失条件；未提供价格输入时不虚构金额。计划绑定配置、release 和实际资源/state revisions，注明哪些检查尚待实际创建后执行。
2. **apply**：校验计划仍适用并取得对应锁；分阶段创建基础设施、准备节点/依赖、安装应用，保存原生工具的操作/资源引用。状态已变化时要求重新计划，不执行过期计划。
3. **reconcile**：中断后用同一 installation/operation ID 查询实际资源再继续。云 API 回执未知时先对账，不能更换名称创建重复集群；阶段失败不自动销毁已有数据或外部复用资源。
4. **verify**：在隔离测试 Workspace 创建 Computer，认证连接、操作浏览器、写文件、提交成果并从独立客户端读回；生产配置还验证受控跨节点重建。清理测试资源，保留脱敏报告。

202/运行中必须返回可查询 ID；`status` 的成功响应只证明查询成功，operation 的 pending/blocked/failed 不能显示为部署已完成。重复 apply 不重复初始化文件系统、旋转秘密或迁移数据库。并发安装者通过 state/release 锁与明确管理权拒绝冲突。

## DP05. 能力预检与安装验收

静态预检与实际探测分别报告 `passed/failed/blocked/not_run`。没有权限检查某项时记 blocked，不推定通过；生产探测不得对客户活跃数据做破坏实验。

| 边界 | 必查内容 |
| --- | --- |
| 节点与运行时 | 精确 OS/内核/架构/containerd/runsc、RuntimeClass handler、调度到合格节点、Chromium 内部 sandbox、限制与取消 |
| 文件路径 | 可信 CSI/FUSE、授权子目录、跨节点 close/open、fsync/rename/锁/配额、缓存失效与独立读回 |
| 持久服务 | DB TLS/角色/同步确认；S3 分段上传、完整性、读回及错误；无数据库/对象盘依赖自身 JuiceFS 的循环 |
| 用户连接 | DNS/TLS/OIDC、独立/嵌入登录、WSS/SSE 代理、重连、撤权、网关排空；首版无 WebRTC/TURN 前置 |
| 隔离与运维 | 默认拒绝网络、云 metadata/集群访问阻断、秘密隔离、健康/指标/备份入口和资源配额 |

只有 RuntimeClass YAML 不代表节点已安装运行时。[Kubernetes RuntimeClass](https://kubernetes.io/docs/concepts/containers/runtime-class/) JuiceFS 默认 CSI 路径依赖节点组件；不能为适配受限 Serverless 平台而给用户应用添加 privileged/FUSE 权限。[JuiceFS CSI](https://juicefs.com/docs/csi/introduction/)

组件认证环境须执行 CS09 的故障实验；生产每次安装只跑隔离、受控的现场检查，报告引用适用的认证证据。安装 smoke 不能代替发布时的 T01–T36。

## DP06. 升级、扩缩容与回退

- **版本锁**：记录应用/Chart/schema、Kubernetes、内核、runsc、Chromium/Playwright、CSI/JuiceFS、数据库/S3、工具与 Skill 的版本/digest，以及本次适用测试证据。发布件附签名、校验和、SBOM、来源和已知限制；离线校验使用预置可信根。
- **兼容升级**：先验证协议/schema 兼容窗口和容量，控制服务滚动升级；执行池停止接新任务、通知活动会话、分批排空。旧 Computer 继续固定镜像与驱动，停止并重建后才切换；连接网关更新允许重新授权重连，不承诺长连接永不中断。
- **数据迁移**：使用有版本、互斥与结果记录的显式迁移任务，优先 expand→兼容运行→迁移验证→contract。任意服务副本启动不得隐式改 schema；旧副本未退出前不删除其依赖字段。
- **破坏性升级**：计划标明停写/停机边界和备份，确认旧执行停止或 fence 后再迁移。支持的升级跨度、前向修复/回退条件均在发布清单中列明。
- **回退**：应用回退先检查数据库和数据格式兼容；不可逆迁移按 DP07 恢复，Helm rollback 不代表数据库副作用被撤销。[Helm hooks 的生命周期边界](https://helm.sh/docs/topics/charts_hooks/)
- **扩缩容**：控制服务容量与 Sandbox 容量独立管理。执行池依据排队、可分配资源、启动时间和 AR05 的准入策略扩容；缩容先排空、保存检查点并验证挂载释放/fencing，不仅看 CPU。需要强停时明确原因和未保存范围。

## DP07. 备份、恢复、卸载与诊断

备份集记录控制库恢复点、JuiceFS 元数据恢复点、被引用对象/检查点/版本、release lock、密钥版本引用与保留策略。时间戳相近不证明跨系统一致；必要时暂停写入，恢复后逐项核对清单/对象，GC 在备份保留窗口内保护其引用。JuiceFS 在线 dump 不保证快照一致性，不能作为活跃数据库一致备份的替代。[元数据备份](https://juicefs.com/docs/community/metadata_dump_load/)

恢复遵守 D11：隔离旧权威/旧节点→恢复匹配数据→校验引用→更新恢复 epoch/凭据→重建→查询未知外部副作用→开放受控写入。先在一次性环境演练并记录实际 RPO/RTO；不宣称恢复了未提交编辑、进程内存或外部网站状态。

卸载应用默认保留 Workspace、成果、数据库、对象和外部依赖。永久清理使用独立计划，展示归属、活跃引用、备份/保留影响与不可逆项；外部复用资源不随安装卸载。诊断包含脱敏配置、版本、阶段、资源引用和错误，不默认包含文件正文、DOM、截图、Cookie 或 state 中的秘密。

## DP08. 交付顺序与完成证据

先完成版本清单/预检、应用 Chart、同构单节点和已有集群路径，再认证一个新建云环境及多节点升级恢复；离线作为单独认证项，云市场仅包装已认证交付件。首版不建设通用多云平台或安装专用 Operator。

每条宣布支持的路径必须交付配置 schema/样例、固定发行包、资源归属表、升级兼容矩阵、操作手册和 T28–T30 证据。验证包括干净安装、重复/并发 apply、不同阶段中断恢复、上一受支持版本升级、活动会话维护、备份恢复和保留数据卸载。所有运行证据目前均缺失，本文完成只代表部署设计已补齐。
