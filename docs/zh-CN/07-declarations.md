# 07. ComputerSet 声明验证

## 07.1 使用

在仓库根目录运行：

```bash
bazel run //:agent-computer -- validate examples/research.computer.yaml --json
bazel run //:agent-computer -- schema computer-set --json
```

CLI 接受文件或 `-`（stdin），可指定 `--format yaml|json`。未指定时，`.json` 文件按 JSON 解析，其余输入按 YAML 解析。YAML 是 JSON 兼容的数据子集；拒绝自定义 tag、多文档和重复 key。示例的镜像摘要是占位值，静态校验不会拉取镜像、创建资源或读取凭据。

退出码：0 静态有效；1 声明无效；2 用法或输入读取错误。`--json` 在 stdout 输出单个报告，普通模式的错误写 stderr。错误不会回显源内容、未知字段值或秘密；语法诊断提供可用的行列位置，结构诊断指向导出的 Schema，语义诊断使用 JSON Pointer。

## 07.2 结构与语义

[JSON Schema](../../schemas/computer-set-v1alpha1.json) 由 Rust DTO 生成，覆盖类型、必填项、枚举和未知字段拒绝；附加语义由同一 crate 的验证器执行。使用 Schema 验证通过后，仍须运行 `validate`。

1. 固定 `apiVersion: agent-computer/v1alpha1`、`kind: ComputerSet`，声明不得携带 status、Pod UID、组织/主体身份、原始 env 或 secrets 字段。
2. Volume、Workspace、Sandbox、App、Agent 和 Computer 均可独立声明。资源名在 kind 内唯一，使用 1–63 个小写字母、数字或连字符，首尾为字母或数字；更新前置 `expectedRevision` 必须是正的有符号 64 位整数，创建时省略。
3. 本地引用是同文档相应 kind 的名称；既有资源用 `id:<opaque-id>`，ID 为 1–128 个 ASCII 字母、数字、下划线或连字符。不存在的本地引用拒绝；既有 ID、storageClass、networkPolicyRef、profileRef 和 secretRefs 在报告中列为 `external_references`。
4. 引用边按 Computer → App → Sandbox → Workspace → Volume 分层，Agent 可引用 Sandbox；当前 schema 无法表达回指高层的循环依赖。每个 Computer 必须包含其本地 App 的 Sandbox，可写挂载只能指向主 Workspace。
5. Sandbox 当前只识别 `gvisor` 声明；OCI 镜像按 `oci-spec` 解析并要求固定小写 SHA-256 摘要，不接受浮动 tag。CPU、内存和 Volume 配额为正数；Volume 仅接受 Retain，Workspace 仅接受 explicit 冲突策略。
6. `mounts` 仅接受 workspaceRef/path/readOnly，路径在 `/workspace`、`/inputs`、`/app` 或 `/data` 内规范化且不能重叠。创建真实 Candidate、独立可写 inode 和网络隔离由后续适配器实现。
7. Browser 使用 `chromium-playwright`，必须显式指定 profileRef，启动参数、健康及 profile 路径由 Driver 管理；不能覆盖为 `--no-sandbox`。`web-application` 必须提供 argv、cwd 和健康检查，不能使用 profileRef；statePaths/exportPaths 是显式路径白名单。健康端口非零，路径为规范化 HTTP 绝对路径，启动预算为 1–3600 秒。
8. Agent 可省略。`external` 不带 sandboxRef，`hosted` 必须绑定 Sandbox；当前声明识别 `tools-api` 适配契约。capabilities 只是所需能力，不授予权限。secretRefs 仅接受引用，不接受明文 secrets 字段。

## 07.3 摘要与预算

成功报告返回 `scope: static`、`definition_digest`、资源数和待核验引用。`valid: true` 仅证明本地结构和静态语义；服务端还必须检查引用的类型、组织、ACL、revision、镜像/驱动及部署兼容性。本地 CLI 的远程 plan/apply 仍标记为 unsupported，声明发布使用[控制 API](10-plans-and-apply.md)。运行能力仍未支持。

规范化版本为 `agent-computer/definition-v1`：补齐 DTO 缺省值；按名称排列资源、按 path 排列挂载、按字典序排列引用和路径集合；capabilities 使用 Schema 枚举顺序；保留 argv 顺序。将对象键排序后输出紧凑 UTF-8 JSON，再计算 `SHA256(版本字符串 + NUL + JSON)`。摘要包含名称和 expectedRevision，代表完整声明意图，不等于单个资源的运行 spec digest。YAML 注释、空白、对象 key 顺序和无序集合重排不改变摘要。

输入上限 1 MiB，展开后的 key/字符串累计上限 1 MiB；最多 65,536 个值节点、32 层嵌套、1,024 个资源、每个 Sandbox 128 个挂载，最多返回 100 条诊断并显式标记截断。拒绝重复键，不采用覆盖规则。YAML alias 同样计入展开预算。

## 07.4 验证记录

覆盖人独立声明、未知字段/能力、引用错配、跨组织语法、资源与挂载约束、driver 字段冲突、重复键、alias 展开、摘要稳定性、Schema 同步和 CLI 退出码。Python 的独立 Draft 2020-12 校验器验证示例/未知字段拒绝，独立 SHA-256 计算与 Rust 示例摘要一致。

解析采用 [serde_yaml_ng](https://docs.rs/serde_yaml_ng/0.10.0/serde_yaml_ng/)，镜像格式采用 [oci-spec Reference](https://docs.rs/oci-spec/0.10.0/oci_spec/distribution/struct.Reference.html)。crate 版本由 Cargo.lock 固定，Bazel 的 crate_universe 从同一工作区和锁文件生成依赖。
