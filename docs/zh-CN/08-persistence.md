# 08. PostgreSQL 声明持久化

## 08.1 已交付范围

内部 `Store::record` 方法将已静态验证的 ComputerSet 文档保存到内部 PostgreSQL 声明库。每个组织、文档名有仅追加的版本历史与当前 revision，为后续计划保留提交意图；此方法不解析外部引用、不创建独立资源 SpecVersion、不授权 apply、不写协调任务，也不启动 Computer。该可信内部方法使用独立的声明库前置条件。[授权 plan/apply 路径](10-plans-and-apply.md)现可在同一声明库事务内发布资源版本和协调意图：根 `metadata.expectedRevision` 约束声明当前版本，每个资源另用自身 revision 条件。HTTP 不调用未鉴权的 record 方法。

服务必须为每次调用认证授权，包括读取、幂等记录退役和事件确认。`Store` 接收可信 SQLx 连接池；部署凭据、TLS、连接上限与超时仍由服务负责。此库不是 HTTP 或 CLI 数据库接口。数据库访问方必须可信；查询中的组织条件不等于 PostgreSQL 行级安全策略。

## 08.2 事务契约

`record` 要求组织、主体、幂等键、已验证文档，以及显式 `Create` 或 `Match(revision)` 前置条件。库自行计算操作、前置条件和规范化文档的摘要；规范字节包含目标名与全部声明字段。幂等作用域为组织 + 主体 + 操作 + 键。

单个事务取得组织事件流行锁、检查已有请求回执、检查当前 revision，再一起提交不可变版本、当前指针、回执、事件和 Outbox。相同意图重试返回原回执，即使当前版本已推进；改变意图返回 `IdempotencyConflict`，已退役键返回 `IdempotencyGone`，不能复用。事务失败不留下键占用、部分版本或事件。

序号由普通加锁行分配，因此同组织的后续写入不能先于较小序号提交。不同组织有独立计数器。PostgreSQL 行锁保持到事务结束；代价是同组织声明写入串行化。依据见 [PostgreSQL 行锁](https://www.postgresql.org/docs/18/explicit-locking.html#LOCKING-ROWS)。

不可变版本保留准确的规范字节和摘要。SQL 触发器拒绝 UPDATE/DELETE，外键保证当前指针引用已有版本。Cargo 与 Bazel 均嵌入迁移 SQL；SQLx 验证迁移历史和校验和，并锁住并发迁移。依据见 [SQLx Migrator](https://docs.rs/sqlx/0.8.6/sqlx/migrate/struct.Migrator.html)。数据库所有者权限不受这些应用保护措施约束。

数据库错误可能让客户端无法确定 COMMIT 是否到达服务器。调用方必须以相同意图和键重试并恢复回执；错误不转换为成功，也不自动允许换新键重新执行。

## 08.3 快照、重放与 Outbox

`snapshot` 在一个只读 REPEATABLE READ 事务内读取当前文档和 watermark。`replay(after, limit)` 在同类快照内读取保留边界、watermark 和有序事件页，每页允许 1–1000 条。返回的 `next_cursor` 是实际返回的最后一条事件，限量分页不会跳到更后的 watermark。游标早于保留边界返回 `CursorExpired`；负数或未来游标无效。后续 API 须将游标绑定到已认证作用域。

事件仅包含声明名、revision、摘要与序号，不复制文档内容。Outbox 在显式确认前重复返回待投递项。投递器必须在下游接受后确认，下游按组织 + 序号去重。多个投递器可能重复或乱序投递；目前没有外部投递进程，也不提供 exactly-once 保证。

显式清理仅删除已确认事件及其 Outbox，并原子推进保留边界；版本历史和请求记录继续保留。请求退役删除回执但永久保留键墓碑。当前快照一次加载组织内所有当前声明；生产快照分页、存储配额和自动保留调度尚待实现。

## 08.4 真实数据库验证

安装 PostgreSQL 18 服务端/客户端二进制，以非 root 用户运行。测试默认路径 `/usr/lib/postgresql/18/bin`，其他安装位置使用 `AGENT_COMPUTER_PG_BIN`。解包安装还可通过 `AGENT_COMPUTER_PG_SHARE` 指定 share 目录，并提供必要的共享库路径。测试不连接现有数据库 URL。

```bash
export AGENT_COMPUTER_PG_BIN=/usr/lib/postgresql/18/bin
bazel test //... --test_env=AGENT_COMPUTER_PG_BIN --test_output=errors
cargo test --workspace --locked
```

如果设置了 share 或共享库路径，Bazel 另传 `--test_env=AGENT_COMPUTER_PG_SHARE`、`--test_env=LD_LIBRARY_PATH`。缺少二进制会令测试失败，不跳过数据库检查。每个测试在私有 `/tmp` 目录启动独立临时集群，仅开放 Unix socket，保留 fsync/synchronous_commit/full_page_writes，退出时停止并删除。Bazel 测试依赖本机安装的外部二进制；目前未提供封闭可复现的 PostgreSQL 分发。

8 个集成场景覆盖并发迁移与校验和拒绝、不可变历史及 WAL 崩溃恢复、作用域/意图/墓碑隔离、并发重试和 CAS、末尾写入故障的事务回滚、组织提交锁、写入中的快照一致性，以及重放/Outbox 保留边界。实测基线为 Linux x86_64、PostgreSQL 18.6。证据限于本地数据库行为；复制切换、磁盘/断电故障、生产数据库角色、备份和完整 T01–T43 运行验收仍未验证。
