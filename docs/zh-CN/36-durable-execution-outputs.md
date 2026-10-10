# 36. 有界执行输出持久化

一次性执行 worker 现在会持久保存已收到的 supervisor 最终报告，控制器退出后仍可读回。原始报告、保留的 stdout、stderr 和 supervisor 诊断分别以内容摘要寻址存入 S3，PostgreSQL 保存引用及有界流元数据。这仅表示输出发布：执行仍为 `Unknown`、writer 仍为 `Draining`，不会生成完成或排空凭证。

## 36.1 捕获与发布

首次捕获要求原始内存派发句柄和私有 attach 观察。收集器校验已登记的 Pod UID、派发、启动授权、watchdog 布防、挑战、请求摘要及 generation。严格解析报告，检查保留字节上限和截断计数。报告自称成功时，还必须具有退出码零、无 signal、后代已回收以及两个输出流 EOF。这些检查不构成权威执行完成。

迁移 19 新增不可变的 `execution_output_intents` 和 `execution_outputs`。意图将四个对象引用绑定到组织、执行及原始派发/授权/布防/Pod；外部发布前，与 `execution.output_pending` 事件一同提交。迁移不会根据历史授权推断输出。

收集器将原始字节写入独立的私有本地 spool，依次执行独占文件创建、文件 fsync、目录 fsync、不可覆盖的原子重命名及父目录 fsync。读取拒绝符号链接、错误所有者、组或其他用户权限、硬链接、长度或摘要不符；重试会校验已有文件。spool 必须使用运维所有的持久本地存储，与租户 Candidate 和 watchdog 日志目录分离。

对象键为 `execution-outputs/v1/<organization>/<execution>/<sha256-hex>`。存储身份摘要包含 endpoint、region、bucket，凭据轮换不改变该身份。客户端显式配置凭据，使用 SigV4 和 path-style 地址。默认要求 HTTPS，仅隔离测试可显式开启 `allow_http`。禁止重定向、环境代理和自动传输重试；每请求连接超时 2 秒、总超时 10 秒。当前 bucket 名只支持小写字母、数字和连字符，不支持会话凭据和 endpoint 路径前缀。

条件 PUT 使用 `If-None-Match: *`。写入成功或对象已存在后，必须完整 GET 并验证长度和 SHA-256；ETag 不作为内容证据。缺失对象只能使用原始、再次验证的 spool 字节重建。冲突、错误、对象不符和不确定确认均保留为未确认发布。四个引用全部读回通过后，在一个事务中登记发布及 `execution.output_verified`。恢复重试返回相同时间戳，不重复发送输出事件。

worker 给收集流程 30 秒预算；取消 future 不能中断已运行的文件系统调用或阻塞 spool 任务。若在完整 spool 重命名前崩溃，可能留下没有本地可恢复字节的意图。恢复可利用 S3 中已存在且验证通过的对象，但不能补造缺失字节或重执行任务。spool/对象保留和 GC 仍由运维负责，当前不自动删除。

## 36.2 配置与读取

私有 worker 配置的 `execution` 中新增两个必填字段：

```json
{
  "output_spool": "/var/lib/agent-computer/outputs",
  "outputs": {
    "endpoint": "https://objects.example.invalid",
    "region": "us-east-1",
    "bucket": "execution-outputs",
    "credentials_file": "/etc/agent-computer/output-credentials.json",
    "ca_file": null,
    "allow_http": false
  }
}
```

以 0700 创建 spool 目录。凭据文件是仅所有者可访问的私有普通 JSON 文件，包含 `access_key`、`secret_key`；派发前配置 bucket 权限和保留策略。可选私有 `ca_file` 会替换 HTTPS 信任根。配置无效或 spool 不可访问时，在创建持久执行派发意图前拒绝派发。升级时须给旧配置补齐这两个字段。

`GET /v1alpha1/executions/{id}/output` 要求 `runtime.connect` 和原连接的有效凭据。捕获前返回 null；之后返回 `pending`/`verified`、manifest 摘要、观察到的结果、stdout/stderr 摘要、保留/观察字节计数、截断/EOF 标记和发布时间，不暴露对象地址或字节。`verified` 表示历史发布检查通过，不保证当前可用性，也不代表权威成功。

具有数据库和对象存储权限的可信运维可使用同一私有配置：

```sh
agent-computer-server execution-output-recover --database-url-file /private/database-url \
  --organization org_id --execution-id execution_id --config-file /private/worker.json
agent-computer-server execution-output-read --database-url-file /private/database-url \
  --organization org_id --execution-id execution_id --config-file /private/worker.json
```

恢复会重新检查所有引用，使用原 spool 重试缺失上传。读取要求已有发布记录，再次验证完整报告对象后将原始 JSON 写入 stdout。两个命令均不依赖 Kubernetes 连接，不创建、attach 或执行 Pod。报告含用户 stdout/stderr，应私下保存；常规派发命令 JSON 仅返回输出元数据及 `output_unconfirmed`，省略原始观察。

## 36.3 范围与验证

每流保留上限沿用原命令配置，最大 1 MiB。JSON 报告上限为 `8 * cap + 16384` 字节，诊断上限为 64 KiB；观察计数可超过保留上限。当前收集最终报告，连续分块流及控制器在捕获前丢失的报告仍待实现。

测试覆盖私有 spool 重开/篡改、存储身份绑定、SigV4、条件 PUT、丢失确认、损坏/截断/超量 GET、重定向、鉴权和服务故障、SQL 引用移植拒绝、不可变发布、WAL 恢复及历史迁移。真实一次性 PostgreSQL/SeaweedFS/K3s/CSI/gVisor 环境验证正常输出、运维派发、双流截断、S3 凭据拒绝及数据库发布失败。全新运维进程在 Kubernetes 凭据不可用时恢复两个 pending 场景，并重复恢复，确认不增加授权或事件。

权威完成仍需物理存储 fencing 和 writer 排空。按 [JuiceFS CSI 架构](https://juicefs.com/docs/csi/introduction/)，使用同一 PV 的 Pod 可能共享 CSI 客户端；删除 Pod、进程死亡或 cgroup 为空不能证明该客户端写入已排空。本轮不宣称产品 T01–T43 验收完成。

2026-10-10 的[源码绑定证据](../evidence/durable-execution-outputs-2026-10-10.json)和[日志](../evidence/durable-execution-outputs-2026-10-10.log)记录了 327 项默认测试通过（含 141 项 PostgreSQL 用例）、11 个 Bazel 测试目标及 11 个真实 worker 场景。五份发布的二十个引用由独立 S3 客户端再次校验；两个发布故障恢复后均未增加授权或重复事件。已删除本轮虚拟机、测试服务及私有磁盘/密钥文件。
