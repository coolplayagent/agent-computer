# 14. Candidate 存储准备

## 14.1 已交付范围

`agent-computer-storage` 在受信任的 JuiceFS 挂载中准备独立 Candidate 文件：验证固定 manifest，设置目录配额，逐个复制并核验对象摘要，同步文件及目录，最后通过禁止覆盖的原子重命名发布私有准备收据。这是 Linux 运维库和命令，尚不是租户 API，也不授予运行写权限。

控制数据库仍须提供并授权 organization、Volume、Workspace、Candidate、Computer 和 generation。[15 运行 grant](15-runtime-authorization.md) 与 [16 启动排队准入](16-start-admission.md) 已实现；后续 [17 Candidate 准备 worker](17-candidate-preparation-worker.md) 已实现消费准备证据的事务。修改租约、fencing 与 Pod 挂载准入仍待实现。既有 Workspace 声明 Schema 未变；内部请求的 quota 不是新增的公开声明字段。调用者须先完成 Artifact 授权及对象存储读取，将输入放入私有缓存；知道对象摘要不等于具有读取权限。

## 14.2 存储契约

准备器要求完整 JuiceFS 1.4.1 文件系统挂载、S3 数据后端、匹配的文件系统 UUID，并关闭 JuiceFS writeback 与 FUSE writeback。生产构造器拒绝普通本地目录。运维人员在挂载下提供既有、独占且由准备器用户所有的 `0700` Volume 准备目录；其父路径、配置、可执行文件与缓存的祖先目录必须由运维控制。应用最终仅接收 `data` 叶目录。

Volume 准备目录内采用以下结构：

```text
organization/<org>/workspace/<workspace>/candidates/<candidate>/generation/
  staging_<random>/             # 保留未完成或竞争失败的尝试
    data/
    receipt.json
  <generation>/                # 原子发布，永不覆盖
    data/                      # 唯一可供后续应用挂载的目录
    receipt.json               # 位于应用挂载之外
```

ID 为有界的不透明字符串。请求和 manifest 拒绝未知字段。Manifest 支持目录及具有精确字节数、SHA-256、可执行标记的普通文件。相对路径最多 1,024 字节、32 段，每段最多 255 字节；拒绝路径穿越、空段、反斜杠及 NUL。最多接受 10,000 条目及 10,000 个不同目录。按 4 KiB 保守估算的分配量须落在整数 GiB 配额内；这只是输入限额，不是并发总用量预留。

基于目录描述符的 `openat2` 禁止符号链接和跨挂载点访问。输入缓存必须是仅有一个硬链接的普通文件，设备、FIFO 等对象在实际打开 I/O 前即被拒绝。输出创建为独立 inode，不依赖硬链接或写时复制。未来写入用户获得文件所有权，普通文件为 `0600`，可执行文件和目录为 `0700`；控制父目录保持准备器私有。

配额适配器通过 `juicefs status` 核对元数据文件系统 UUID，并在读取任何输入前等待 `juicefs quota set` 成功。它使用运维固定摘要的可执行文件、结构化参数、私有密码文件及有界命令超时，不把取整后的配额表或 CSI 异步配额标记作为确认。发布后还须在最终路径再次设置配额才返回成功。JuiceFS 客户端缓存配额计量，因此这不证明跨节点逐字节准入；参见上游[配额指南](https://juicefs.com/docs/community/guide/quota/)与固定版本[配额实现](https://github.com/juicedata/juicefs/blob/v1.4.1/cmd/quota.go)。

每次写入、文件 `fsync` 和目录同步必须成功。发布使用 `renameat2(RENAME_NOREPLACE)`。精确重试核对保留的请求、写入用户、Volume 路径绑定、文件系统 UUID 和 data inode，再确认配额；不会重新读取基线对象或覆盖已修改的工作文件。同一 Candidate/generation 下改变 manifest、Computer、配额或写入身份均冲突，新 generation 则使用独立存储。即使上次重命名成功而最终配额确认失败，收据也仅是存储证据。

失败和竞争落败的暂存目录均保留。本增量没有自动删除、接管半成品、孤儿预算预留或垃圾回收；运维必须计入这些保留字节。在准入和清理实现前，不应将重复准备暴露为无界租户派发。

## 14.3 运维命令

使用 `bazel build //crates/storage:agent-computer-storage` 或 `cargo build -p agent-computer-storage --locked` 构建。运行环境须为支持 `openat2`、禁止覆盖重命名和目录同步的 Linux。准备器需要设置所选非 root UID/GID 的权限。应用 Pod 不得获得完整文件系统或元数据凭据。

运维配置示例，所有部署占位值均须替换：

```json
{
  "mount_root": "/private/mounts/juicefs",
  "volume_path": "actual-pv-subdirectory/preparation",
  "filesystem_uuid": "actual-filesystem-uuid",
  "volume_uid": "actual-volume-uid",
  "object_cache": "/private/objects",
  "writer_uid": 1000,
  "writer_gid": 1000,
  "quota": {
    "executable": "/private/bin/juicefs",
    "executable_sha256": "sha256:replace-with-64-lowercase-hex-digits",
    "metadata_url": "postgres://metadata@metadata.example/juicefs?sslmode=verify-full",
    "password_file": "/private/secrets/metadata-password",
    "timeout_seconds": 30
  }
}
```

首版适配器接受无内嵌凭据、仅含一个 `sslmode` 参数的 PostgreSQL URL。生产 TLS 证书分发仍是部署责任；本地组件实验只在一次性虚拟机内部使用 `sslmode=disable`。配置及密码文件必须是当前调用用户所有的私有普通文件，最多 8 KiB；请求和独立 manifest 文件最多 4 MiB。它们均须只有一个硬链接；错误输出不包含原始路径、后端输出或凭据。

最小空 manifest 为 `{"entries":{}}`，保存为私有文件后计算规范摘要：

```bash
agent-computer-storage manifest-digest --file /private/manifest.json
```

将结果填入私有请求文件：

```json
{
  "organization": "org_example",
  "volume_uid": "actual-volume-uid",
  "workspace": "workspace_example",
  "candidate": "candidate_example",
  "computer": "computer_example",
  "generation": 1,
  "quota_bytes": 1073741824,
  "manifest_digest": "sha256:replace-with-manifest-digest",
  "manifest": {"entries": {}}
}
```

文件条目为 `{"kind":"file","sha256":"sha256:<64 hex>","size":123,"executable":false}`，输入字节置于 `<object_cache>/<64 hex>`；目录条目为 `{"kind":"directory"}`。Manifest 摘要是对规范 JSON 值执行带域分隔的 SHA-256，应使用命令计算，不能直接哈希文本文件。

```bash
agent-computer-storage prepare \
  --config-file /private/storage-config.json \
  --request-file /private/prepare-request.json
```

成功输出一条 JSON 收据。退出码 1 表示结果尚未确认，需对同一身份继续对账。收据不证明旧写入者已停止，也不授权新写入者。

## 14.4 验证与限制

22 项默认存储测试覆盖独立文件、重试保留修改、generation 隔离、绑定冲突、输入完整性与大小限额、复制前及发布后的配额失败、并发发布、符号链接/硬链接/FIFO 拒绝、inode 替换、私有控制路径、挂载资格和配额命令失败/超时。执行 `bazel test //crates/storage:storage_contracts_test` 或 `cargo test -p agent-computer-storage --locked`。

一次性虚拟机探针还对真实 JuiceFS/PostgreSQL/S3 运行 Rust 命令，覆盖目录发布、仅叶目录 bind mount 下的非 root 修改、重试、generation 隔离以及发布路径的 `EDQUOT`。组件结果不证明运行授权、fencing、断电恢复、分布式配额准入或完整 T01–T43 验收；真实后端故障导致的文件同步失败与多节点恢复仍须故障注入证据。

## 14.5 已记录组件实测

2026-10-09，`98e1831` 的 Bazel 二进制在隔离 Ubuntu 虚拟机中通过 JuiceFS 1.4.1、PostgreSQL 16.15、SeaweedFS 4.48 实测。1 MiB 输入复制为独立 Candidate generation；UID 1000 写入者通过 data 叶目录 bind mount 修改 generation 1，精确准备重试保留修改，改变 Computer 则冲突。发布目录的 1 GiB 配额拒绝超额写入，并在 `fsync` 与关闭时返回 `EDQUOT`。

另一个使用全新内存缓存的只读 JuiceFS 客户端核验了 generation 2 输入的 SHA-256，指标记录 1 MiB 对象 GET 数据。[固定源码证据](../evidence/candidate-storage-2026-10-09.json)保存二进制/源码摘要、探针脚本、请求、观测及限制；[日志](../evidence/candidate-storage-2026-10-09.log)保存命令与测试输出。采集后已销毁虚拟机及其私有数据。这些是受信任运维组件探针，应用运行准入和验收仍待实现。
