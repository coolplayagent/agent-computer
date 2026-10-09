# 20. Candidate 有界文件保存

## 20.1 已实现路径

可信 `candidate-file-save-once` worker 可消费连接所有的修改租约，保存一个文件，将 [19 写入租约](19-candidate-writer-leases.md)接入实际存储 IO。能力查询返回 `candidate.file_save: trusted-worker`；可选 [21 HTTP 网关](21-file-http-gateway.md)支持有界读取/保存，目录列表及人的文件界面仍待实现。

派发前重新检查当前 owner/generation/epoch/revision，并将配置中的完整 Volume target 与不可变准备绑定比较。挂载的 JuiceFS 必须符合已记录 UUID/PVC 和同步上传配置。文件操作边界再次核对私有准备收据、Candidate inode、data 的 UID/GID 和 mode。调用者不能选择宿主绝对路径；授权沿用租约获取所需的独立 Computer 与 Workspace grant。

每次保存最多 1 MiB 二进制内容，已有文件也须在该上限内。路径沿用相对路径限制：最长 1024 个 UTF-8 字节、32 个分量、每分量最多 255 字节；父目录必须存在。拒绝点/双点、空分量、反斜杠、NUL、符号链接穿越、hardlink、FIFO、把目录当文件及跨挂载访问。以 `.agent-computer-write-` 开头的名称保留给服务使用。

## 20.2 版本比较与替换

`expected: null` 要求目标不存在；否则 `expected` 必须给出准确的旧 `sha256`、`size` 和 `executable`。这是内容/执行位前置条件，相同内容与执行位具有相同版本。冲突返回当前版本且保持目标不变，不静默覆盖不同版本。

适配器基于固定目录描述符打开文件，独占创建新的暂存文件、设置属主/mode、同步文件，再检查旧版本，原子 rename 并同步父目录，读回 bytes/hash/mode 后才报告 Applied。替换产生新 inode，已打开的读者继续读取原文件。该网关要求所有写者遵守 Candidate 独占租约，不声称隔离已持有可写挂载的任意进程。

执行 IO 前，数据库先记录规范化文件意图和准备收据的摘要。许可只能消费一次，其保守单调时钟截止时间包含派发往返耗时。同步适配器不向外泄漏进程、任务或可写文件描述符；worker 等待实际阻塞调用返回，取消 async future 不能作为排空证据。卡住的 FUSE 操作可能超过期限，其间所有权仍不可接管。

## 20.3 命令与结果

使用私有配置、凭据和请求文件。配置包含 `mount_root` 及 [17 Candidate 准备](17-candidate-preparation-worker.md)中的准确 `target`，不包含 object-cache 或 quota 配置，因为已准备 Candidate 已有配额。凭据必须是原连接凭据。请求为严格 JSON，包含二进制字节数组编码后最多 5 MiB。

```sh
agent-computer-server candidate-file-save-once \
  --database-url-file /run/secrets/agent-computer/database-url \
  --credential-file /run/secrets/agent-computer/connection-credential \
  --lease-id lease-example \
  --config-file /run/secrets/agent-computer/file-worker.json \
  --request-file /run/secrets/agent-computer/file-save.json
```

```json
{
  "lease": {
    "connection_session_id": "connection-example",
    "generation": 1,
    "epoch": 1,
    "expected_revision": 1
  },
  "dispatch_id": "save-example-1",
  "edit": {
    "path": "note.txt",
    "expected": null,
    "content": [72, 105, 10],
    "executable": false
  }
}
```

命令返回当前租约视图；命令本身成功退出不等于文件保存成功，须检查 `file_edit.state` 与租约 `state`：

| 文件结果 | 含义及所有权 |
| --- | --- |
| Applied | 文件/目录同步和读回确认，数据库接纳时授权有效；有界写者排空，租约 Released |
| Conflict | 修改前发现旧内容/执行位不匹配；有界写者排空，租约 Released |
| Expired | 修改前本地期限已过；有界写者排空，租约 Released |
| Unknown | IO 或回执未确认，或完成写入后、接纳结果前授权失效；另检查 `drain_confirmed` |

不确定 IO 的 `drain_confirmed=false`，不生成释放证明，租约继续 Draining。若同步写入已完成但授权随后被撤销，接纳结果为 Unknown，已封闭执行路径的排空证据仍可释放所有权；这不会撤回文件变化。不接受客户端自报停止。通用进程/节点 fencing 仍是独立后续工作。

每个 epoch 只允许一次保存。已确认保存、冲突或修改前到期通过 `bounded_file_drained` 证明关闭当前 epoch；下一次保存需重新获取租约。`GET /v1alpha1/leases/{id}` 与 `writer-lease-reconcile` 返回可空的 `file_edit`，不返回文件正文或宿主路径。同派发 ID 和文件意图的准确命令重试只返回已记录结果，不重复 IO。已有派发但没有完成记录时绝不重新签发许可；改换意图失败。进入新 epoch 后旧请求失败。

## 20.4 持久化与验证

迁移 11 增加不可变完成记录，绑定派发、epoch、输入摘要与准备摘要。事务原子记录实际观察/接纳结果、事件/Outbox、排空及释放证明，并在末尾重查授权和期限；失败事务不改变这些记录。存活 worker 可使用同一封闭执行结果重试持久化，不重复 IO；若在提交证据前死亡，派发仍未解决并阻止接管，恢复需后续权威排空/fence 证据。不确定修改留下的暂存文件保留诊断，不自动清理。

新增 7 项本地文件系统测试覆盖原子替换、旧读者 inode、二进制/Unicode/空/上限文件、旧版本、恶意对象、Candidate 身份和修改前到期；新增 2 项 PostgreSQL 场景验证未知派发重试及升级安全。默认 Cargo/Bazel 检查共 224 项，不能仅凭这些测试认证 JuiceFS。显式 `candidate_worker_live_test` 另覆盖真实 CSI/准备、CLI 保存/重试、替换、冲突、IO 后撤权、Outbox 提交失败及不确定 IO 后禁止交接；仅在一次性环境按第 17 节配置运行。

2026-10-09，该显式测试在提交 `06f3d14` 上通过，使用一次性 KVM 虚拟机及真实 PostgreSQL、JuiceFS CSI 和 S3。[固定源码组件记录](../evidence/candidate-file-save-2026-10-09.json)包含二进制/源码摘要、探针脚本和[运行输出](../evidence/candidate-file-save-2026-10-09.log)。六次文件派发形成六条不可变完成/事件/Outbox 记录及五条有界排空证明；第六次拒绝符号链接目标，保留 Unknown/Draining 并阻止接管。新的只读 JuiceFS 客户端不使用磁盘缓存，读回最终 23 字节文件且 SHA-256 与记录一致；连同准备恢复文件，共发生两次 S3 GET、读取 50 字节。随后删除虚拟机、可写磁盘和私有凭据。该实测未覆盖 FUSE 挂起、节点/进程故障、断电或通用物理 fencing。

目录列表、大文件流、目录操作、受监督进程/watchdog、物理 fencing、Artifact 发布及 Computer Ready 仍待实现。T01–T43 完整运行验收继续为 `not_run`。
