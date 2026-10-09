# 计算与存储技术选型对比

版本：设计基线 0.5；资料核对日期：2026-10-09；状态：设计决策，尚未完成组合实测。

本文展开 ADR-02、ADR-03、ADR-05、ADR-06、ADR-07、ADR-09 的比较依据，先分析约束和候选，再给出选择。产品边界以[产品定义](../requirements/agent-computer.md)为准；接口、生命周期与提交协议以[详细设计](../design/agent-computer.md)为准；验证入口是[验收设计](../test/agent-computer.md)中的 T00、T03、T08、T09、T16、T19、T20、T25–T27。

表中的组件能力引用官方资料；“更适合本项目”“集成成本较高”等结论是结合本项目约束的工程判断。成本是构成分析，不是报价；性能是影响机制，不是未运行的基准分数。官方页面和主分支可能变化，实施前必须锁定 release、镜像 digest 与配置重新验收，不能将本文核对日视为版本兼容清单。

## CS01. 需求、假设与决策方法

### 工作负载决定比较对象

| 工作负载 | 计算特征 | 存储特征 | 对选择的影响 |
| --- | --- | --- | --- |
| 人与 Agent 使用浏览器 | 长时进程、多个子进程、实时画面与输入、外部网页 | Cookie/profile 含数据库与锁；文件上传下载 | 不能只优化短函数冷启动；需测试浏览器 sandbox、交接和停止检查点 |
| Shell、构建与资料处理 | 不可信生成代码；CPU、系统调用、网络和小文件负载混合 | 大量 stat/open/rename，候选分支与提交 | 单一 CPU 基准无法说明实际性能；原生路径接口与一致性重要 |
| 生成 Web 应用的运行体验 | 独立 Browser 与应用服务，应用端口私有，人的活动保活 | 固定输入只读、试用状态隔离、显式保存新成果 | 要有应用健康、网络边界和运行预算，不能靠 task 终态销毁环境 |
| 成果、检查点与执行日志 | 提交、恢复与读取可异步，交互关键路径不能无限阻塞 | 不可变字节、大小/hash 校验、保留与 GC | 与可写文件系统分开，备份必须覆盖清单、字节及加密密钥 |

已确定：私有多机 Linux；本地单节点同构；多人/Agent 接续；单 Candidate 修改者和单 GUI 控制者；文件/检查点恢复，首版不承诺内存迁移。尚未知：真实并发、文件数量、数据量、现有集群/NAS/S3、是否暴露 KVM、运维团队能力和价格。因此不假设已有 Ceph，也不声称任意机器都适用同一部署包。

### 比较次序

| 优先级 | 维度 | 判断方式 |
| --- | --- | --- |
| 硬约束 | 私有部署与数据控制 | 能在组织授权环境部署；外部服务有明确的数据去向和依赖 |
| 硬约束 | 隔离、权限与故障隔离 | 不可信工作负载不得接触宿主管理接口；旧执行者可实际停止/fence |
| 硬约束 | 一致性与恢复 | 不发布缺字节成果，不因换节点丢失已确认状态，不允许旧代次重新写入 |
| 硬约束 | 工作负载兼容 | Browser、Shell、文件、WebApplication 与活动会话保护都能实现 |
| 高 | 运维与集成成本 | 控制面数量、故障域、备份链、升级、值班能力、需自研的适配代码 |
| 高 | 交互性能与资源效率 | 首帧、输入回执、冷/热启动、文件尾延迟、单位有效会话资源占用 |
| 中 | 扩展与可迁移性 | 增加节点/容量、复用现有基础设施、迁移字节/元数据、避免私有接口扩散 |
| 中 | 总成本、维护与分发 | 闲置资源、存储副本/备份、对象请求/网络、支持与维护状态、具体版本许可 |

先排除不能满足硬约束的配置，再在可行方案间比较；不能用“启动快”抵消成果丢失或权限旁路。尚无基准与采购价格，不做带小数的主观总分。CS08 用不同部署条件检查结论是否变化，CS09 给出补齐实证的方法。

## CS02. 计算：调度、隔离和恢复分别比较

### 调度与部署控制面

| 方案 | 私有/本地与长时会话 | 多节点、网络与存储 | 本项目需补齐的工作 | 运维、成本与迁移判断 |
| --- | --- | --- | --- | --- |
| Docker Compose + 自建节点 worker | 单机容易起步，容器可以长时运行 | Compose 单机使用不等于多机调度；扩展为 Swarm 或自研控制面是另一项工程 | 节点发现、放置、配额、失联处理、网络与挂载协调；Computer 语义仍需自己实现 | 初始组件少，但自研调度和故障恢复的长期成本高；适合组件开发与单机验证，不作为多机主方案。[Docker 文档](https://docs.docker.com/compose/how-tos/production/) |
| Nomad | 可私有部署，支持常驻任务；服务端/客户端架构 | 有调度、共识与 CSI，不是“没有存储生态” | 为本产品实现 allocation 映射；逐项验证所需 CSI、网络策略和隔离驱动组合 | 已有 Nomad 平台时很有竞争力；从零引入又实现一套与 Kubernetes 不同的适配路径，当前收益不充分。[架构](https://developer.hashicorp.com/nomad/docs/architecture)、[CSI](https://developer.hashicorp.com/nomad/docs/architecture/storage/csi) |
| 新建 K3s + Kubernetes API | 同一 API 覆盖本地单节点与私有集群，常驻 Pod 可支撑人的会话 | 复用调度、RuntimeClass、CSI 和网络插件；HA embedded etcd 需至少 3 个 server | Computer 协调器、真实 fencing、活动保护和产品授权；这些不由 Kubernetes 自动提供 | 比单机 Compose 多运维；但不用自研通用调度器。K3s 是 Kubernetes 发行方式，不是另一种隔离技术。[K3s 架构](https://docs.k3s.io/architecture)、[HA](https://docs.k3s.io/datastore/ha-embedded) |
| 复用已有 Kubernetes | 可沿用组织控制面和运维；需有受控运行时节点池 | 可复用已验收的 CSI、网络及监控，但集群存在不代表 runsc 已可用 | 能力探测、节点隔离、RuntimeClass、挂载和策略验收 | 已有合格平台时优先于再建 K3s；减少重复控制面，不降低验收标准。[RuntimeClass](https://kubernetes.io/docs/concepts/containers/runtime-class/) |

Nomad 与 Kubernetes 都需要产品级连接、租约和成果服务；比较的是适配和运维成本，不能把一方的底层能力等同于整个 Computer 产品。Nomad 当前主分支许可证是 BUSL-1.1；选定分发版本时须读取对应许可和例外条款，本文不把它等同于传统宽松开源许可，也不据此推断所有用途受限。[Nomad LICENSE](https://github.com/hashicorp/nomad/blob/main/LICENSE)

### 现成 Sandbox 平台能否直接替代

E2B 是值得比较的完整运行平台，其官方 runtime 提供 Firecracker、模板、进程/文件 API、路由、暂停恢复和持久卷，既有 Cloud/Enterprise，也有自有硬件上的 Embed；不能笼统说它只能用公有云。当前 README 将单机 Embed 明确定位为评估包，并列出 PostgreSQL、Redis、ClickHouse 等组件。[E2B Runtime](https://github.com/e2b-dev/runtime)

| 比较维度 | 采用 E2B 类平台 | 本项目自己持有 Computer 控制面 |
| --- | --- | --- |
| 交付速度 | 可复用沙箱、模板、执行和路由；原型集成工作可能更少 | 资源适配需要实现，但直接表达 Computer/Workspace/Presentation 契约 |
| 私有部署与责任 | Cloud、专属部署、Embed 的运维/支持范围不同，必须针对交付形态判断 | 由组织控制基础设施，需自己承担版本、备份和节点运维 |
| 会话与恢复 | 生命周期 API 提供连接、暂停/恢复和超时配置；套餐/部署限制需匹配人的长时使用 | 文件/检查点恢复与人的活动保护由本产品控制，不要求依赖内存快照 |
| 权威与锁定 | 仍需补人/Agent 共用控制租约、固定成果提交、跨主体授权；避免与平台各写一份实例权威 | Computer 保存产品状态，Kubernetes/运行时仅报告实例；适配边界清楚 |
| 结论 | 作为后续 ComputeBackend/快速验证候选，不直接认定已满足本项目全部契约 | 首版先采用 Kubernetes 后端；若目标改成最快交付托管服务，应重新评估整个平台复用 |

E2B 生命周期限制与可配置行为以其[官方 lifecycle 文档](https://docs.e2b.dev/sandbox)及所选部署版本为准；本文不引用套餐价格或把云端限制套用到自托管版本。

### 隔离运行时

| 方案 | 隔离边界与宿主要求 | Linux/浏览器兼容路径 | 性能与密度的影响机制 | 集成和适用性 |
| --- | --- | --- | --- | --- |
| 原生 OCI 容器 / runc | namespaces/cgroups/seccomp 等，共享宿主内核；无需虚拟化 | Linux 调用最直接，通常是兼容性基线 | 不增加 guest kernel 或用户态内核层；实际资源仍由浏览器/工具决定 | 可以运行可信控制组件和对照基准；本设计不将其作为不可信工作负载的默认隔离边界。[Docker 安全](https://docs.docker.com/engine/security/) |
| gVisor / runsc | 用户态应用内核；Systrap 不要求 KVM，适合未开放嵌套虚拟化的宿主 | 需重新实现 Linux 接口；实际 Chromium、进程、挂载组合要验证 | 增加系统调用、文件和网络路径开销；不能用 CPU 算术基准推定构建/网页表现 | OCI/containerd 接入自然，保留容器资源模型；是本项目当前密度、宿主覆盖与隔离之间的候选折中。[架构](https://gvisor.dev/docs/architecture_guide/intro/)、[性能机制](https://gvisor.dev/docs/architecture_guide/performance/) |
| Kata Containers | 轻量 VM、独立 guest kernel 与硬件虚拟化；需合格 KVM/宿主 | Linux 内核兼容路径更接近普通 VM，但设备、挂载、guest 配置仍要测 | guest kernel、VM 管理与文件共享有额外开销；不先验断言一定比 runsc 慢 | 经 OCI/CRI/containerd 接入，适合作为硬件隔离或 runsc 不兼容时的替代；不是自动 fallback。[Kata 架构](https://github.com/kata-containers/kata-containers/blob/main/docs/design/architecture/README.md) |
| 直接管理 Firecracker microVM | KVM + 最小设备模型，需维护 guest image、jailer、网络与块设备 | 可在 guest 运行 Chromium；远程画面不需要 VMM 自带桌面显卡 | 适合精简 VM 和快照路径；端到端启动还包括镜像、文件恢复与 Browser 就绪 | Firecracker 本身不是完整调度、文件共享或连接产品；直接采用要额外构建这些能力，当前范围成本高。[Firecracker 设计](https://github.com/firecracker-microvm/firecracker/blob/main/docs/design.md) |
| 每台 Computer 一台完整 VM | guest OS 与虚拟化边界；管理能力取决于 IaaS/虚拟化平台 | 适合完整 OS、原生桌面或特殊驱动需求 | 要计入 OS 维护、guest 常驻资源、镜像与启动成本；已有 VM 池可改变成本 | 后续完整桌面/特殊环境候选；首版 Browser + WebApp 的收益不足以默认承担完整 OS 管理 |

gVisor、Kata、VM 都不解决授予沙箱过多数据、越权文件挂载或应用自身泄露；不能用隔离名称替代 ACL/网络配置。也不能把“gVisor 不覆盖所有 Linux 接口”写成“它不能运行浏览器”。Browser 内层 sandbox 必须保留；目标组合不兼容时先阻断该配置，再验证 Kata 等替代，不添加 `--no-sandbox` 或 privileged 取得假兼容。

### 恢复路径

| 方案 | 能恢复什么 | 成本、兼容与风险 | 本项目取舍 |
| --- | --- | --- | --- |
| 文件 + 应用检查点，重建进程 | 已保存工作文件、Artifact、正常关闭后的 profile/应用状态 | 重新启动与观察有延迟；不能恢复任意用户态缓冲 | 与既定产品承诺一致；易跨可兼容节点迁移，也便于版本/权限审计 |
| VM/进程内存快照 | 可保留更多进程内部状态，减少部分初始化 | 额外内存/磁盘快照、CPU/内核/设备兼容、凭据过期与恢复隔离；外部网站副作用不回滚 | 若未来要求秒级接续且基准证明有价值，再单独设计；不因 Firecracker/E2B 支持就直接增加产品承诺 |

以上是恢复语义比较，未声称已测启动时延；无论哪条路径都要查询外部真实状态，不能重放未知提交。

## CS03. 控制元数据：事务数据库还是 KV/单文件

控制库需要一次提交资源 revision、lease/generation、执行状态、Artifact 清单与 Outbox，并支持按主体、资源、状态查询。它不存大文件正文；产品库、文件系统元数据库和 Kubernetes 自身的集群数据库是三个不同权威域。

| 方案 | 事务/并发及查询 | 多实例与恢复 | 运维与适配成本 | 结论 |
| --- | --- | --- | --- | --- |
| PostgreSQL | 事务、行锁与条件更新适合 CAS；关联查询适合审计和列表 | 多 API 实例通过数据库协调；WAL/复制/备份可用，但 HA 需正确配置 | 需运维数据库与迁移；可与 JuiceFS 复用数据库技术而非共用表 | 默认选择。优势来自契约匹配和减少数据库种类，不是宣称比所有数据库更快。[锁](https://www.postgresql.org/docs/current/explicit-locking.html)、[复制](https://www.postgresql.org/docs/current/warm-standby.html) |
| MySQL/InnoDB | 同样有事务与锁，能实现 CAS/Outbox，功能上可行 | 需独立验证复制、故障切换和提交确认配置 | 已有 MySQL 团队时有价值；换用需改迁移、SQL 和并发验收 | 有效备选；当前没有必须同时维护第二 SQL 后端的需求。[InnoDB 事务](https://dev.mysql.com/doc/refman/8.4/en/innodb-transaction-model.html) |
| SQLite | 嵌入式事务库，单文件便于开发；并发写有单写者约束 | 放共享盘不变成多机数据库；WAL 要求同机共享内存 | 部署最简单；但远程协调服务/复制另做后就不再是简单嵌入模式 | 不作多 API 控制库；可用于单进程工具内部状态。拒绝的是这项部署用途，不是 SQLite 的可靠性。[适用场景](https://www.sqlite.org/whentouse.html)、[WAL](https://www.sqlite.org/wal.html) |
| etcd | 线性一致 KV、比较事务、watch 与协调能力 | 适合小规模协调元数据，需 quorum、压缩和备份 | 审计列表/多关系查询需自行组织索引；再配 SQL 会产生双权威和跨库提交 | Kubernetes 内部继续使用；不另用它承载全部产品对象及日志。[etcd 能力](https://etcd.io/docs/v3.6/learning/why/) |

选择 PostgreSQL 并不自动获得“主节点故障不丢已确认提交”。生产配置须区分本地 WAL 落盘与副本确认：异步复制可能丢失已提交事务。需要故障后保留已确认状态时，控制库和 JuiceFS 元数据库分别使用有同步副本确认的写入配置，故障切换只提升符合持久边界的副本；同步副本不可达时接受写阻塞。自动降为异步会改变 RPO，不能静默执行。[PostgreSQL 同步复制](https://www.postgresql.org/docs/current/warm-standby.html#SYNCHRONOUS-REPLICATION)

因此本地可使用单 PostgreSQL，但只用于同构开发，不把单节点称为生产 HA；K3s 本地用 SQLite/生产用 etcd 的选择也不改变产品控制库的 PostgreSQL 契约。

## CS04. 工作文件：共享文件系统、块盘与同步方案

Browser、Execution 和 WebApplication 可以在不同节点运行。共享文件方案要支持受控跨节点访问，业务冲突仍由 Candidate、单写租约和 CAS 处理。RWX 只描述可挂载能力，不等于允许任意并发覆盖；RWO 是单节点读写，并非严格单 Pod。[Kubernetes PV 语义](https://kubernetes.io/docs/concepts/storage/persistent-volumes/)

| 方案 | 文件语义与多节点 | 性能影响机制 | 耐久、备份与运维 | 对本项目的匹配 |
| --- | --- | --- | --- | --- |
| JuiceFS CE + 元数据库 + S3 | POSIX 接口与 Kubernetes CSI；数据块/路径元数据分离 | FUSE、元数据 RPC、S3 请求和缓存参与；小文件、冷读、fsync 是重点 | 需同时维护元数据库和对象字节，单备份 bucket 不够；计算节点可替换 | 适合存算分离、跨节点工作目录及复用对象存储；默认候选，但必须联合验证 runsc/CSI。[架构](https://juicefs.com/docs/community/architecture/)、[CSI](https://juicefs.com/docs/csi/guide/pv/) |
| CephFS | 基于 RADOS 的共享 POSIX 文件系统；MDS 管文件元数据 | 元数据热点、网络、OSD 布局和恢复流量影响尾延迟 | 要维护 MON/MGR/OSD/MDS、容量与故障域；已有 Ceph 时增量运维可低 | 已有成熟 Ceph 平台时优先评估直接使用，避免为工作文件再叠一层 JuiceFS；从零建设负担较大。[CephFS](https://docs.ceph.com/en/tentacle/cephfs/)、[架构](https://docs.ceph.com/en/tentacle/architecture/) |
| NFSv4.1 + HA NAS | 标准远程文件访问、状态/锁与缓存协议；可供多节点挂载 | RTT、NAS 控制器/磁盘、目录负载、客户端缓存决定表现 | HA、快照、复制、配额取决于 NAS 实现；一台 NFS server 不等于 HA | 已有可靠 NAS、规模有限时可能比新建分布式存储更省；不能仅凭协议名推定 NAS 可扩展性或恢复能力。[NFSv4.1 标准](https://www.rfc-editor.org/rfc/rfc8881.html) |
| 网络块卷 + ext4/XFS | 普通文件系统适合单拥有者；RWO/RWOP 挂载受驱动约束 | 文件操作路径直接，但重新挂载/attach 和卷重建增加恢复时间 | 块副本/快照由后端负责；旧节点 fence 和一致性快照仍要处理 | 适合数据库/profile 的专用卷候选；不直接满足不同节点的 Browser/Execution 同时共享目录 |
| 节点本地盘 + rsync/打包上传 | 本地语义与热性能好，但节点间不是同一活动文件视图 | 运行时少远端 I/O；交接成本随文件数量和变更量增长 | 节点丢失前未上传内容可能丢；需自己解决增量同步、删除、冲突和交接协议 | 可用作临时构建/cache；不作本项目持久共享 Workspace 默认方案 |
| S3 直接挂载 / s3fs | 对象映射为文件外观，不等同完整共享 POSIX | 重写/rename 可能转为对象复制；目录操作依赖远程请求 | s3fs 明确不提供原子 rename 和多客户端协调 | 可用于特定只读分发；不用于有 CAS 候选、工具锁和原子文件操作需求的工作目录。[s3fs 限制](https://github.com/s3fs-fuse/s3fs-fuse#limitations) |
| AgentFS / SQLite 文件系统 | 侧重 Agent 文件、状态和调用历史的单库表达，也有挂载/SDK | 快照、分支与审计有吸引力；多节点并发访问需独立服务/协议证明 | 需要明确数据库并发、共享服务、恢复和大文件容量路径 | 保留为每任务独立状态/存储适配研究，不能由可携带数据库推定已有分布式共享文件服务。[AgentFS](https://github.com/tursodatabase/agentfs) |

“共享工作目录”与“每个任务打包交换成果”是不同产品语义。若以后仅需独立短任务，节点本地盘 + 固定对象输入输出可能更划算；当前不能为了省去共享存储而取消人和 Agent 对同一工作环境的接续。

### JuiceFS 元数据后端的第二层比较

| 元数据方案 | 主要收益 | 代价与约束 | 本项目判断 |
| --- | --- | --- | --- |
| PostgreSQL / MySQL | SQL 运维、备份与持久化能力可复用 | 元数据吞吐和延迟仍须实测；不能与业务表混写 | 选独立 PostgreSQL 数据库/账号；和控制库共用技术，不共享权威 |
| Redis / Valkey | 内存型元数据路径，可作为性能备选 | 内存容量、noeviction、持久化与故障切换需单独配置；Redis Cluster 不自动把单个 JuiceFS 文件系统拆散到多个 slot | 只有实测证明 SQL 元数据成为瓶颈时再评估，首版不为未经证实的性能收益新增一类数据库 |
| TiKV | 分布式事务 KV，可横向规划元数据 | 新增分布式数据库集群、资源和运维 | 超出单库能力且有证据时再引入 |
| SQLite / 嵌入式后端 | 单机实验简单 | 本地数据库不自然提供多节点共享服务 | 不作生产共享 Workspace 元数据 |

这些后端及约束来自[JuiceFS 元数据引擎文档](https://juicefs.com/docs/community/databases_for_metadata/)。此处选择 PostgreSQL 是运维简化判断，未宣称 SQL 元数据性能优于内存或分布式 KV。

### 缓存、挂载与隔离必须联合判断

- JuiceFS 客户端 `--writeback` 会把上传移到异步路径；本项目禁用它以及延迟上传，确认写入必须跨过所需持久边界。FUSE 的 `writeback_cache` 是另一个机制，不能混为一谈；首版不额外启用它，任何缓存调优必须重新验证一致性。[JuiceFS cache](https://juicefs.com/docs/community/guide/cache/)
- CSI/FUSE 挂载由可信节点组件完成，用户沙箱只接收授权目录。不能为挂载方便将 FUSE 管理凭据或 privileged 权限交给生成代码。
- gVisor 对外部可变 bind mount 应保留 shared 模式；不能把 Workspace 设置为 exclusive 来取得较好基准。其文档明确独占缓存用于外部修改目录会产生陈旧状态或错误。[gVisor 文件系统](https://gvisor.dev/docs/user_guide/filesystem/)
- “JuiceFS 支持 POSIX”和“gVisor 支持挂载”分别成立，不证明组合满足 fsync、rename、锁和交接。验收必须穿过真实 CSI → 节点挂载 → runsc → 用户进程链路。

## CS05. Artifact/Checkpoint：接口、存储实现与不可变性

### 为什么成果使用独立对象存储

| 方案 | 固定版本和大字节 | 恢复/权限/成本影响 | 选择 |
| --- | --- | --- | --- |
| S3 对象 + SQL 清单 | 适合按键写入、分段上传、hash 校验和独立读取 | 对象先上传、清单后发布；GC/ACL/引用关系由产品管理；计入请求、网络和备份 | 首选；与 D07 提交协议匹配，不要求把共享目录永远封死 |
| 共享文件系统上只读目录 | 也能保存成果，但只读策略和永久保留要单独实现 | 与活跃文件系统的故障/运维耦合，跨产品下载与保留需额外网关 | 可作特定部署适配，首版不维护第二条成果后端 |
| PostgreSQL bytea/大对象 | 可把小内容与元数据一起事务化 | 大输出增加数据库、WAL、复制和备份压力 | 仅元数据/小型内联字段；不把视频、归档和大日志放控制库 |

S3 是访问协议选择，不是自动获得所有 Amazon S3 保证的品牌替代。Amazon S3 的读后写一致性和版本能力有官方承诺；其他兼容实现必须逐项验证，不能继承这些承诺。[Amazon S3](https://docs.aws.amazon.com/AmazonS3/latest/userguide/Welcome.html)

### 对象存储实现

| 实现/交付方式 | 私有部署与维护 | 功能/一致性证据 | 成本与运维权衡 | 本项目判断 |
| --- | --- | --- | --- | --- |
| 组织已有合格 S3 服务，包括许可范围内的托管服务 | 可复用现有运维；数据位置、私网连接和账号策略必须满足部署要求 | 以实际厂商、版本、配置为准；不是所有服务都等同 AWS | 少维护一套存储，但有存储量、请求、流量与服务依赖成本 | 首先复用；仍执行本项目协议/故障验收，不另装存储集群 |
| Ceph RGW | 可自建；已有 Ceph 时复用同一存储运维体系 | 官方 S3 能力表列出支持/部分支持项；版本化、multipart 等按 release 核对 | 新建需管理完整 Ceph；已有 Ceph 时增量服务通常更合理 | 已有 Ceph 的优先对象后端；不要为了只提供 S3 就忽略底层集群成本。[RGW S3 API](https://docs.ceph.com/en/tentacle/radosgw/s3/) |
| SeaweedFS | 可私有部署，Apache-2.0 项目；包含 master、volume、filer、S3 等角色 | 上游有 S3 接口文档；“提供 S3”不能代替一致性、multipart、版本和故障实验 | 可组合所需角色，但生产 HA、filer 元数据、卷复制/恢复仍须配置和运维 | 无既有 S3 的新建私有部署，选为首个参考实现候选，先做 CS09 验证；尚非已认证产品依赖。[项目](https://github.com/seaweedfs/seaweedfs)、[S3 API](https://github.com/seaweedfs/seaweedfs/wiki/Amazon-S3-API) |
| MinIO 社区仓库 | 官方仓库显示 2026-04-25 已归档，只读；仓库标注 AGPLv3 | 历史 S3 能力不等于当前持续维护 | 新项目需自行承担维护来源评估；不能把商业产品支持推定给社区归档版本 | 不选作新增默认参考实现；已部署系统或商业支持版本另按准确产品、维护契约验收。[官方仓库](https://github.com/minio/minio) |
| 单机本地目录/S3 mock | 开发容易，无多故障域冗余 | 可测 API 形状，不能证明跨节点耐久 | 成本低，但机器/盘丢失即破坏生产目标 | 仅开发/单元验证，不作为多机生产对象存储 |

选择 SeaweedFS 作为新建参考候选的理由是可自建、所需对象 API 和可评估的分发形态，不是未经测量的“比 Ceph 快”。它仍会增加元数据及副本管理，若连这些运维能力也不具备，就不应承诺自建 HA；应使用组织可运维的现成服务。SeaweedFS 的原生文件接口不是本轮默认 Workspace 的另一个入口，禁止绕过 JuiceFS 元数据直接修改底层文件块。

Artifact 不可变由固定对象引用、提交协议与禁止覆盖共同保证。Bucket versioning 可以帮助恢复误删，但“读取某个 key 的最新版”仍不是固定 ArtifactVersion。需要版本 ID 的后端将 version ID 写入引用；Object Lock/WORM 是额外保留能力，不是首版所有 bucket 的强制前置。JuiceFS 块、成果/日志、profile 检查点应分 bucket/prefix、凭据和 GC 策略；可共用物理存储，但不能声称因此消除了共同故障域。

## CS06. Browser profile、应用数据库与缓存

| 方案 | 正常运行 | 换节点/节点丢失 | 成本与边界 |
| --- | --- | --- | --- |
| 本地活动副本 + 加密应用检查点 | 单实例拥有普通本地文件系统，适合 profile/SQLite 的活动锁与 WAL | 正常关闭后上传再恢复；节点突然丢失只能回到最近检查点 | 默认；接受明确 profile RPO，不能以 Workspace 的耐久承诺覆盖它 |
| 独占网络块卷 | 保留普通文件系统与单拥有者；可能减少节点丢失的状态损失 | attach/detach、旧节点 fence、崩溃恢复均要验证 | 需要更低状态丢失窗口且已有可靠块存储时的替代；卷快照仍需考虑应用一致性 |
| 共享 RWX 目录上直接运行多份 profile/SQLite | 文件可见不等于锁、共享内存和应用支持成立 | 恢复时容易出现旧实例并存和状态冲突 | 不采用；SQLite WAL 明确依赖同机共享内存，多节点共同打开不适用 |
| 仅进程/临时盘，无检查点 | 最少持久化工作 | 退出即丢全部会话状态 | 可作明确无状态模板，不作持久 Computer 的默认体验 |

依据是[SQLite WAL 的同机约束](https://www.sqlite.org/wal.html)与本项目单实例恢复契约。profile RPO 改善优先考虑应用检查点策略或独占卷，不能通过把活跃数据库复制到共享目录伪造可靠性。构建缓存、下载缓存和可重建中间文件使用有配额的节点 SSD；它们丢失只影响性能，不得改变已提交成果事实。

## CS07. 组合比较与最终选择

单组件胜出不代表组合最优：JuiceFS 依赖数据库和对象存储；Kata 会改变挂载路径；已有 Ceph 会改变运维成本。以下是完整组合层面的判断。

| 组合 | 硬约束适配 | 初始投入和持续运维 | 最大风险/代价 | 当前结论 |
| --- | --- | --- | --- | --- |
| Compose/runc + 本地盘 + 定期上传 | 默认隔离边界与活动共享文件不满足既定目标 | 试验起步最简单；多机能力要自研 | 用短任务原型替代持久交互电脑 | 不作为产品主线；仅组件开发 |
| Kubernetes/K3s + runsc + PostgreSQL + JuiceFS + S3 | 设计上匹配私有多机、持久文件与独立实例 | 复用通用调度，新增文件元数据/CSI 运维；已有 S3 时更合算 | Chromium/runsc/CSI 组合兼容、小文件开销、双层持久化与恢复 | 首版参考架构，受 CS09 的实测门限制 |
| Kubernetes + Kata + CephFS/RGW + PostgreSQL | 在有 KVM、合格 Ceph 与运维时同样匹配 | 已有 Ceph/KVM 时成本可控；从零部署组件多 | VM/共享文件路径与 Ceph 故障域运维 | 强替代组合；非当前自动降级路径 |
| Kubernetes + runsc/Kata + HA NFS + S3 + PostgreSQL | 合格 NAS 与挂载语义验收后可匹配 | 已有 NAS 时部署可能更省 | NAS 实现差异、吞吐/故障切换边界 | 已有 NAS 环境的适配优先项，不宣称当前已支持 |
| E2B 专属/runtime + 自有 Computer 权威/成果层 | 需补齐并证明共享文件、人的控制与稳定身份映射 | 复用较多执行能力；同时维护或依赖平台控制面 | 双层实例状态、内存快照模型、部署/服务契约和迁移 | 保留平台复用路线；目标变为托管快速交付时重新排序 |

据此选择如下，括号中是承担的代价：

1. **调度采用 Kubernetes API，新建环境默认 K3s，已有合格 Kubernetes 则复用。** 因为产品需要多节点放置、运行时/存储插件和长期应用生命周期；相比自研节点调度，适配工作更集中。（承担集群运维与控制面资源成本，不将 Kubernetes 当成产品权限库。）
2. **不可信 Browser/Execution/WebApplication 默认 containerd + gVisor Systrap。** 在尚未保证 KVM 的 Linux 环境中可增加应用内核边界，同时保持 OCI 接入；比直接管理 microVM 少一套 guest/块设备/路由生命周期。（承担系统调用兼容和 I/O 开销；未通过测试即阻断，不降为 runc。）
3. **控制元数据选 PostgreSQL；JuiceFS 元数据也选 PostgreSQL，但分库、账号和备份责任。** 单事务可表达产品状态、CAS 与 Outbox，又减少数据库技术种类；MySQL 同样可行，但当前没有引入第二套实现的需求。（承担数据库 HA、迁移与同步写的延迟/可用性取舍。）
4. **共享工作文件默认 JuiceFS CE + CSI + S3，保留条件化的 CephFS/NFS 适配方向。** 符合跨节点、存算分离与对象后端复用；相比本地同步，不需要自己再实现共享活动视图。（承担 FUSE、元数据和对象请求成本；不宣称对小文件或低延迟负载普遍优于 CephFS/NFS。）
5. **Artifact、日志、Checkpoint 用独立 S3 对象 + PostgreSQL 清单。** 共享可变目录和固定成果各有语义，固定大字节不挤入控制库。对象服务首先复用已有合格 S3；全新私有部署首个参考候选是 SeaweedFS，已有 Ceph 则优先 RGW。（S3 兼容程度、备份与耐久须针对所选实现验收；社区 MinIO 不作为新增默认。）
6. **profile/本地应用数据库使用单实例活动副本 + 正常关闭后的加密检查点；缓存放节点 SSD。** 适配活动数据库约束，跨节点按版本重建。（接受节点丢失时最近检查点之后的状态可能丢失；若不能接受，转独占块卷方案重新验收。）

此结论是实现和验证的优先顺序，不是“所有候选均已支持”。首版主要实现一套 Kubernetes/runsc/JuiceFS 后端和 S3 适配；替代组合需要独立驱动、配置与兼容证据。SeaweedFS 也只是拟优先验证的参考部署，实现尚未交付。

## CS08. 成本模型、场景敏感性与改选条件

### 应比较完整成本

`总成本 = 控制与存储基础资源 + 活跃/空闲/预热计算 + 文件/成果/检查点/备份容量 + 对象请求与网络 + 运维/支持 + 适配与迁移成本`。

同样的业务 bytes 可能同时存在于候选目录、JuiceFS 块、Artifact、checkpoint、对象版本/备份以及 workflow 导入副本中。不能把对象存储单价直接当作用户成果的实际单位成本。副本/纠删码开销取决于具体配置，不在这里假定固定倍数。

计算侧分别测“每个活跃 Computer 小时”和“每个实际完成场景”的资源/费用，区分 GUI 空闲保活、构建高峰和预热池；热池改善首帧但增加空闲成本。存储侧分别测小文件元数据、冷/热读、完整提交、跨节点恢复和备份恢复。指标要覆盖 Sentry/guest、CSI/FUSE、元数据与对象服务，不能只数用户容器 RSS。

| 条件变化/证据 | 应重新评估什么 | 保持不变的产品契约 |
| --- | --- | --- |
| 组织已有 Kubernetes、Ceph、NAS 或对象服务 | 优先复用合格组件，分别比较增量运维；已有 CephFS 时不强加 JuiceFS | 固定成果、租约、ACL、持久确认与故障实验 |
| 没有 KVM，或运行于不开放嵌套虚拟化的 VM | 保留 runsc Systrap；Kata/Firecracker 不能当现成可用配置 | 不降低隔离边界；兼容不通过则配置不可发布 |
| 必须提供硬件虚拟化边界、特定内核/设备或完整桌面 | Kata 或完整 VM；浏览器/文件/恢复重新验证 | 单控制者、人的独立入口、代次与授权 |
| 实测系统调用/文件开销导致交互或构建预算失败 | 比较 Kata、优化已验证缓存、CephFS/NFS；不要仅开 writeback 换跑分 | 已确认文件和成果的耐久性、共享可见性 |
| 对象延迟、冷启动或小文件请求费用占主导 | 比较 NAS/CephFS，调整输入物化/只读缓存和文件打包策略 | 不将可变 Candidate 伪装为固定 Artifact |
| 实测 PostgreSQL 元数据成为扩展瓶颈 | 先定位连接/索引/热点，再比较 Redis/Valkey、TiKV 或其他 FS | 数据库变更不削弱提交确认与恢复 |
| 业务只剩独立短任务，不再需要同一活动目录 | 可改为本地工作盘 + 对象成果交换，减少共享 FS | 需先修订产品范围，不能在实现中悄悄取消共享电脑 |
| 要求保留进程内存或明显缩短接续时延 | microVM/E2B 快照路线，测量收益及跨版本恢复成本 | 外部副作用仍需对账，凭据和租约仍需重新校验 |
| 没有可运维数据库/对象服务的团队 | 评估组织托管服务或完整专属运行平台 | 不能把单机 demo 标为私有 HA 产品 |

迁移成本也影响结论：运行时切换先排空与检查点，再建新 generation；Workspace 迁移需封存写者、导出普通文件/清单、校验 bytes/hash/权限并切换绑定；更换 S3 要同时迁移所有已引用对象与版本/密钥映射，不能只改 endpoint。旧后端在保留与备份窗口结束前不得提前 GC。公共 Computer/Artifact ID 不因底层后端改变而重新解释。

## CS09. 选型实验与发布阻断条件

下面是设计验收项，**全部尚未执行**。它们补充 T16/T20 的具体方法，不把本轮文档校验升级为运行证明。

| 实验 | 对照与固定输入 | 必须记录/独立验证 | 决策条件与关联测试 |
| --- | --- | --- | --- |
| B01 运行时兼容与隔离 | 同硬件/镜像的 runc 可信基线、runsc；有 KVM 时增加 Kata。Browser、Shell、生成 WebApp 全覆盖 | 内层 browser sandbox 开启，进程树、OOM/取消、DOM/视觉、下载、私网限制、共享挂载；记录宿主/guest/驱动精确版本 | 任一必要功能需 privileged/关闭 sandbox 才能运行，阻断该配置；不因其他吞吐高而放行。T04、T06、T16、T18、T25 |
| B02 交互与容量 | 单节点和至少双 worker；1/10/50 个并发作为实验梯度，受机器资源上限约束；分别冷/热镜像与缓存 | 请求到 Ready、Ready 到首帧、输入回执、p50/p95/p99、失败率；全部基础组件 CPU/RSS、网络、会话时长和预热成本 | 这些数字是实验输入，不是容量承诺；目标预算在测试前固化，不能看到结果后挪门槛。T20、T24–T26 |
| B03 文件系统与双层缓存 | 同样输入/网络下比较 JuiceFS 与可得 NFS/CephFS；1 千/10 万个小文件、1/10 GiB 大文件、真实代码树 | stat/open/rename/fsync、热/冷读取、跨节点新读者、旧 FD/缓存失效、上传下载、提交 hash、配额；禁止 exclusive mount 与异步持久化掩盖错误 | 丢失已确认写入、交接后陈旧读/静默覆盖、逃逸或错误吞没均阻断。T03、T07–T09、T16 |
| B04 对象服务正确性 | 实际部署 S3；新建参考 SeaweedFS 与可得既有服务/RGW 对照；相同对象/分段数据 | Put/Get/Head/Delete、multipart complete/abort、校验和、版本引用、权限、超时与重复请求；断对象节点后从独立客户端读回 | 缺失已发布对象、完整性失败、未授权读取、迟到上传覆盖均阻断；模拟器通过不能替代此实验。T09、T10、T16、T19 |
| B05 数据库与物理 fencing | 两 API/协调器、同步副本配置、数据库主故障、网络分区和旧节点仍存活 | CAS/Outbox 原子性、已确认事务恢复、租约/epoch、不允许旧进程继续写的实际证据；记录写阻塞与切换时间 | 丢确认元数据、双权威、仅删 Pod 就复活旧写者均阻断。T08、T12、T13、T19 |
| B06 检查点和整体恢复 | profile、应用 SQLite、共享目录、成果与密钥清单；正常停止/骤停/换节点；控制库和文件元数据恢复 | 正常关闭后可恢复；崩溃仅恢复最近有效检查点且如实显示；从另一节点验证成果 hash；验证 GC/备份引用 | 不能将活跃 DB 拷贝当检查点，不能仅恢复 bucket 就报全部恢复。T03、T11、T19、T26、T27 |
| B07 维护与完整成本 | 对本次候选冻结软件来源/版本、支持方式、存储冗余、备份和报价输入 | 记录基础资源、请求/流量、重复存储、值班和升级步骤；复盘现有平台的增量成本 | 只有达到相同正确性/隔离门的候选才比较成本；维护中断或升级无法验证触发重选。T15、T20 |

每轮先冻结硬件、内核、镜像、组件版本、配置、并发、输入 manifest、目标预算、样本量和采样窗口，输出原始证据与失败项。基准至少覆盖冷/热两组、多轮重复及稳态；尾延迟报告注明样本数量，样本不足不发布 p99 结论。当前没有组织批准的时延/吞吐/SLA 数字，实施阶段在首次测量前确定目标；硬正确性门不依赖这些待定性能目标。

发布决策依据是“正确性/隔离门通过 + 实际场景满足预先约定预算 + 运维可承担”，不是组件官方 benchmark 的最快条目。若默认组合失败，先记录失败和原因，按 CS08 改选并更新详细设计、版本清单及对应验收；不能删除失败路径来保留原选型。

## CS10. gVisor、JuiceFS 的成熟度与采用条件

两者均有生产使用证据，但成熟度分为项目、具体功能/配置和本项目组合三个层面。当前只有前两层的上游资料，本项目仍没有运行验收；“官方生产使用”不能直接写成 agent-computer 已认证。

| 组件 | 选择理由 | 成熟度证据 | 采用条件与保留问题 |
| --- | --- | --- | --- |
| gVisor | Agent 生成代码、第三方依赖和网页需要隔离；用户态应用内核减少直接暴露的宿主系统调用面，兼容 OCI/containerd；Systrap 不依赖 KVM | Google 的 GKE Sandbox 提供 gVisor；上游有生产、兼容和性能文档 | 首个认证运行时候选；需保留 Chromium 内部 sandbox，验证真实系统调用、构建、小文件、网络、取消和挂载链路；不保证任意 Linux 程序兼容 |
| JuiceFS CE | Browser/Execution/WebApplication 可跨节点访问授权 Workspace；用文件接口复用对象存储，计算节点可替换 | 开源项目有生产说明；官网刊载的阶跃星辰技术分享明确包括社区版生产及规模优化 | 共享工作目录参考后端；元数据库、对象数据、CSI/FUSE 和缓存共同影响性能/恢复；不能将其他后端或企业版案例推定为 PostgreSQL CE 配置已验证 |

gVisor 的事实依据：[生产指南](https://gvisor.dev/docs/user_guide/production/)、[GKE Sandbox](https://docs.cloud.google.com/kubernetes-engine/docs/concepts/sandbox-pods)、[Systrap 平台](https://gvisor.dev/docs/architecture_guide/platforms/)、[兼容性](https://gvisor.dev/docs/user_guide/compatibility/)、[性能机制](https://gvisor.dev/docs/architecture_guide/performance/)。系统调用、网络和文件路径的额外成本与负载相关，不给出未经实验的固定损耗比例；普通 OCI 应用可运行不等于 gVisor 已兼容。需要硬件虚拟化边界、完整内核功能或实际兼容/性能不达标时，按 CS02 比较 Kata/microVM，先适配验收，不自动降级到 runc。

JuiceFS 的事实依据：[社区仓库](https://github.com/juicedata/juicefs)、[阶跃星辰社区版生产分享](https://juicefs.com/en/blog/user-stories/artificial-intelligence-storage-large-language-model-multimodal)、[架构](https://juicefs.com/docs/community/architecture/)、[PostgreSQL 元数据](https://juicefs.com/docs/community/databases_for_metadata/)。案例证明存在实际使用和工程投入，不证明本项目小文件/交互负载、当前发行版本或自建运维已满足要求。

JuiceFS 默认 close-to-open 语义不能解释为所有已打开文件即时全局一致；open-cache 等选项会改变可见性，目录项删除/重建和外部共享挂载还涉及缓存失效。确认写入、交接、原子操作和配额必须走真实 runsc/CSI 路径验证。[缓存边界](https://juicefs.com/docs/community/guide/cache/) 元数据和对象块必须配套备份；在线 dump 不是一致快照，不可用“bucket 完整”替代可恢复性。[备份边界](https://juicefs.com/docs/community/metadata_dump_load/)

人和 Agent 在同一 Computer 内接续本身不要求 JuiceFS。当前选择它的额外驱动力是不同节点上的组件共享持久工作目录。若组织已有可靠 NAS/CephFS，先比较复用；若调整为每个 Computer 的组件始终共置并使用独占持久卷，应同时验证迁移、并行度、跨节点恢复及产品需求，不能仅替换存储名就声称保留全部语义。持久 Artifact 仍使用独立固定对象，活跃 profile/SQLite 仍遵循 CS06。

对外发布口径为“gVisor 为首个隔离认证候选，JuiceFS 为新建共享文件环境的参考候选；支持的是明确版本和配置组合”。云上优先复用合格现有/托管集群与服务，自建 K3s/对象服务只是一条交付路径。具体自动化、状态、升级和认证契约见[DP01–DP08](../design/deployment-automation.md)。
