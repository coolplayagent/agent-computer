# 02. 开发与构建

## 02.1 工具链

基线为 Linux x86_64、Bazel 9.3.0、Rust 1.97.1（edition 2024）和 rules_rust 0.74.0。版本固定于 `.bazelversion`、`rust-toolchain.toml` 和 `MODULE.bazel`；Bazel 模块解析写入 `MODULE.bazel.lock`。首次构建需要下载公开规则及 Rust 工具链，并需要本地 C/C++ 链接工具链。

Bazel 是项目构建与测试入口，直接使用 `rust_library`、`rust_binary` 和 `rust_test`，不通过 shell 转调 Cargo。Cargo 工作区用于编辑器、格式化和 Clippy，二者使用同一 Rust 源码。后续外部 crate 依赖必须保持 Cargo 与 Bazel 锁定一致。

## 02.2 命令

在仓库根目录运行：

```bash
bazel build //...
bazel test //...
bazel run //:agent-computer -- version --json
bazel run //:agent-computer -- capabilities --json
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

安装 Bazelisk 的环境会读取 `.bazelversion`。Rustup 会读取 `rust-toolchain.toml`。工具下载受限时使用组织提供的 HTTPS 代理或经过校验的镜像；不要修改固定版本来掩盖下载失败。

CLI JSON 输出位于 stdout，诊断位于 stderr；不支持的命令/参数返回退出码 2。运行能力尚未交付，不应将版本命令成功当作运行服务健康。

## 02.3 开发约定

每个提交对应可审阅的能力增量，包含相关构建目标、契约测试及两种语言的进度说明。先保持领域状态与外部副作用分离，再加入数据库、适配器和 UI。验证必须覆盖拒绝路径及与现实副作用相关的不变量。

规则配置参考 [rules_rust 官方文档](https://bazelbuild.github.io/rules_rust/)。构建成功只证明当前目标可编译；真实隔离与持久性认证见 [05 验证](05-verification.md)。
