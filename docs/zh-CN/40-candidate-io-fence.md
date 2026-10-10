# 40. 可撤销的 Candidate 文件系统

`agent-computer-fence` 在现有合格 JuiceFS 卷的单个 Prepared Candidate 目录前增加受信 Linux FUSE 挂载，提供该挂载实例的内容与目录变更排空屏障。[执行 CSI 集成](41-fenced-execution-csi.md)已将实际 Pod I/O 接入此挂载；进程执行仍保持 `Unknown`，写租约仍保持 `Draining`，Computer `ready` 仍为 false。

## 为什么需要额外边界

工作负载退出并不证明共享 JuiceFS 客户端已完成写入。文件操作后，JuiceFS 仍可能保留 writer 句柄及后台任务；控制面回执、空进程 cgroup 或 Pod 删除均不能替代存储屏障。上游 [JuiceFS writer 实现](https://github.com/juicedata/juicefs/blob/v1.4.1/pkg/vfs/writer.go)、[VFS 实现](https://github.com/juicedata/juicefs/blob/v1.4.1/pkg/vfs/vfs.go)及 [CSI 挂载共享说明](https://juicefs.com/docs/csi/guide/resource-optimization)描述了这些边界。

新挂载让受信适配器在支持的修改请求进入 JuiceFS 前逐一控制它们。操作使用固定目录和 inode 描述符，租户名称仅允许单路径分量，解析拒绝路径逃逸、符号链接跟随及跨文件系统。挂载仅允许配置的 writer UID 和受信 root 访问，并保留内核权限检查。JuiceFS 操作者控制入口（`.control` 与 `.jfs.*`）属于保留名称，其特殊 inode 范围及其他所有者的底层节点也被拒绝；租户普通 `.config` 目录仍可使用。[合格后端的内部节点契约](https://github.com/juicedata/juicefs/blob/v1.4.1/pkg/vfs/internal.go)说明 `.control` 在子目录下也存在，因此不能当作普通文件暴露。

## 封存与持久化契约

每项内容或目录修改持有挂载操作锁，在底层操作及所需文件、目录同步完成后才回复 FUSE。写入使用可写底层描述符，包含已删除名称的打开文件。新文件在确认前建立归属和权限；复合元数据操作即使后续字段失败，也同步已产生的部分修改。无法确认结果的存储错误会永久阻止正向排空回执。

`MountedFence::seal()` 先不可逆地关闭修改入口，再取得同一操作锁。已接纳的操作必须完成，封存才能成功。排队及之后到达的修改均返回 `EROFS`，包括封存前已打开、或名称已被删除的句柄。底层客户端停滞时，封存也停滞；超时不会被当作成功。读取仍可使用；读取引起的附带访问时间元数据不属于该内容与目录屏障。

返回的不透明 `SealedFence` 绑定已准备目录身份、随机挂载实例及已接纳请求计数。其 JSON `Evidence` 仅为审计元数据，不能重建活的屏障证明。状态标志仅用于诊断，不能证明排空；卸载或进程丢失也不是成功封存。

固定版本的 `fuser` 使用 direct I/O，不协商 writeback cache、passthrough、DAX 或共享可写 mmap。本组件支持普通文件、目录、读写、截断、权限、时间戳、重命名和删除。符号链接、硬链接、设备、扩展属性和共享可写 mmap 尚不可用。上限为 16,384 个保留 inode 身份、4,096 个打开句柄、每次读写 128 KiB；inode 身份在挂载生命周期内保持固定。目录配额仍由 JuiceFS 强制，`statfs` 返回以 Candidate 配额为上限的底层可用空间，并非独立配额预留。

## 受信集成边界

操作者打开现有合格 `MountedVolume`，取得 `candidate_directory(&prepared)`，再把验证后的句柄交给 `agent_computer_fence::mount`。挂载点必须为空、使用绝对路径、归 root 所有；祖先目录也必须归 root 所有且不可由组或其他用户写入。底层描述符和路径必须对工作负载不可见。获得独占写准入的工作负载只能接收此 FUSE 挂载。封存一个挂载不会撤销同一目录的其他挂载、既有直接 CSI 访问或受信代码直接发出的底层写入。

最初的文件系统组件没有新增公共挂载接口或数据库完成转换。第 41 节已增加私有节点发布与执行/Pod 身份绑定；活体 I/O 证明与进程 fencing、受认可完成的组合仍待实现。封存成功本身不授权新 writer，也不证明网络或进程副作用结束。通用应用兼容性还需要解决未支持的文件系统操作。

## 验证

6 项专项单元测试覆盖等待在途 I/O、旧及已删除句柄、真实同步错误、路径/链接/权限拒绝、普通目录错误，以及路径替换后的 inode 固定。全量默认 Cargo/Bazel 测试、Clippy、格式、文档验证和现有 Qualitygate 策略分别记录在[交付证据](../evidence/candidate-io-fence-2026-10-10.json)与[日志](../evidence/candidate-io-fence-2026-10-10.log)。

显式 `candidate_worker_live_test` 使用真实 K3s/CSI、PostgreSQL、JuiceFS 和 S3。在其私有配置中增加 `fence_mount_root`，指向已存在、归 root 所有的目录，如 `/var/lib/agent-computer-fence`。在具备 FUSE 和 `/usr/bin/setpriv` 的一次性 VM 中以 root 运行。工作负载降为 UID/GID 1000 且没有附加组，经新挂载创建和修改文件，保留有名及无名打开句柄，并确认共享可写 mmap 被拒绝。夹具先确认底层 Candidate 确实暴露 JuiceFS 特殊控制 inode，再验证网关在两层目录下均拒绝两种控制名称，同时允许租户普通 `.config/settings`。操作者随后暂停指定原生 JuiceFS 客户端，观察到正在执行的修改，确认封存在 300 ms 内不能完成。恢复客户端后，封存成功，10 项修改操作被拒绝，读取仍得到正确内容；操作者还复核底层字节。另一个关闭数据缓存的只读 JuiceFS 进程在封存后独立读回相同的 7 字节。

这属于单 VM 组件证据，不代表已验证崩溃/断电恢复、HA、产品 Pod 集成、通用进程 fencing 或完整 Computer 验收。T01–T43 保持 `not_run`。
