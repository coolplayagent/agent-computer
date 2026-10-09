# 04. 渐进式实现计划

## 04.1 交付方式

按可审阅、可构建的能力增量提交。每阶段可包含多个提交，前一阶段的测试不能替代后一阶段验收。领域开发可在部署实验同时推进；D1 组合认证仍是运行发行的前置门，不因先编写 Rust 代码而取消。

## 04.2 完整范围与进度

| 阶段 | 交付物 | 对应要求/验收 | 状态 |
| --- | --- | --- | --- |
| 01 | Rust + Bazel、CLI 骨架、编号中英文文档 | R18 / T00 | 已实现；构建、CLI、格式/Clippy、文档检查通过 |
| 02 | 类型化身份、Computer/Lease/Execution 状态约束 | R01、R08、R09、R21 / D02、D04、D06、D08 的领域测试 | 已实现；28 项契约测试通过，真实适配器待实现 |
| 03 | ComputerSet schema/验证器、声明版本、计划与能力协商 | R01、R14、R31 / T01、T17、T36 | 待实现 |
| 04 | PostgreSQL 迁移、原子幂等记录、事件/Outbox、CAS | R06、R08–R10 / T09、T12、T13 | 待实现 |
| 05 | API/OpenAPI、服务凭据/OIDC、授权和多对多 ConnectionSession | R02、R15、R22、R24、R33 / T02、T10、T18、T22、T38 | 待实现 |
| 06 | Kubernetes/gVisor 协调器、实际 fencing、启动/停止/恢复 | R02、R09、R13、R16 / T03、T08、T16、T19；B01–B07 | 待实现/待真实环境认证 |
| 07 | JuiceFS 文件、Candidate、S3 Artifact/Checkpoint 与 GC | R05、R06、R16 / T07、T09、T11、T19 | 待实现 |
| 08 | 受控进程、输出限额、取消、租约 watchdog 与 Unknown 对账 | R04、R08、R09 / T06、T08、T11、T12 | 待实现 |
| 09 | Browser Driver、视觉/结构化动作、profile、控制交接 | R03、R07、R19、R22 / T04、T05、T10、T24 | 待实现 |
| 10 | 独立/嵌入 ComputerView、文件编辑、人的独立使用 | R19、R21–R24 / T21–T24、T26 | 待实现 |
| 11 | Presentation、WebApplication、隔离试用和新版本保存 | R20、R21 / T25–T27 | 待实现 |
| 12 | Helm、部署 CLI、管理界面、预算/计量、备份升级恢复 | R13、R16、R17、R25 / T16、T19、T20、T28–T30 | 待实现 |
| 13 | ContextBinding/委托、ActionIntent/许可、Handoff、Evidence、准入 | R26–R31、R33 / T31–T36、T38–T39 | 待实现 |
| 14 | workflow 与独立工具的版本化适配、恢复和示例 | R11、R12、R14 / T14、T15、T17 | 待实现 |
| 15 | 全核心故障矩阵、多节点实测、性能/成本和发行清单 | R01–R31、R33 / T01–T36、T38–T39 | 待验收 |
| 16 | 可选 teams、开发/资料组合与端到端适配 | R34–R35 / T40–T43 | 待实现；各组合独立认证 |
| 17 | 可选隔离评测 create/reset/step/verify/close | R32 / T37 | 后续扩展，当前 unsupported |

## 04.3 完成条件

完整目标以[产品需求](../../codespec/requirements/agent-computer.md)、[D13 交付顺序](../../codespec/design/agent-computer.md)及[验收门槛](../../codespec/test/agent-computer.md)为准。纯函数单元测试证明局部规则，不证明数据库耐久、进程停止、网络隔离或真实浏览器/人的完整操作闭环。每项运行验收必须保存固定提交、版本、环境、预期/实测结果和证据；未执行或阻断项继续保留。
