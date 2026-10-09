# 05. 验证与交付证据

## 05.1 证据分层

| 检查 | 能证明的范围 | 不能证明的范围 |
| --- | --- | --- |
| Bazel build/test | 当前 Rust 目标编译及实际运行的契约断言 | 外部服务、真实隔离和存储耐久 |
| Cargo fmt/Clippy | 当前源码格式与静态诊断 | 产品业务验收 |
| 文档校验 | 本地链接、双语文件与序号对应 | 所引用设计已经实现 |
| Qualitygate full | 当前策略选中的交付快照检查 | 策略未选择的测试；当前策略仅行尾 |
| relay-knowledge map validate | 映射结构、路由与摘要完整性 | 索引覆盖完整或运行时正确 |
| T01–T43 运行验收 | 各编号在固定真实环境中的实测行为 | 未执行编号或其它部署组合 |

## 05.2 当前记录

2026-10-09（阶段 01–02、03 静态声明部分）：Bazel 构建、CLI JSON 输出与错误退出码、Cargo fmt/Clippy、本地文档链接与双语编号检查通过；Bazel 共通过 63 项测试（28 领域、28 声明、7 CLI 进程测试）。另以 Python 独立验证 Draft 2020-12 Schema 和示例 SHA-256；Cargo 测试与 Clippy 同样通过。运行验收 T01–T43 全部 `not_run`。现有设计检查 T00 不等于产品交付。

本地 Qualitygate 报告保存在已忽略的 `.qualitygate/`；提交前对最终工作区运行 `check --worktree --profile full`。报告需满足非空交付、无 pending checks、`gate.complete: true`、`gate.decision: pass` 与退出码 0，另外执行 Bazel 和静态检查。保留报告的 snapshot/policy digest，不把默认行尾检查扩大解释为功能质量保证。

运行证据字段沿用[验收记录格式](../../codespec/test/agent-computer.md)：`test_id/status/source_commit/component_digests/environment/input_refs/expected/observed/evidence_refs/limits`。当前不会生成虚假的 `passed` 运行报告。
