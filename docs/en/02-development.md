# 02. Development and builds

## 02.1 Toolchain

The baseline is Linux x86_64, Bazel 9.3.0, Rust 1.97.1 (edition 2024), and rules_rust 0.74.0. Versions are pinned in `.bazelversion`, `rust-toolchain.toml`, and `MODULE.bazel`; module resolution is recorded in `MODULE.bazel.lock`. The initial build downloads public rules and a Rust toolchain and needs a local C/C++ linker toolchain.

Bazel is the project build and test entry point, using `rust_library`, `rust_binary`, and `rust_test` directly. It does not invoke Cargo through a shell. The Cargo workspace supports editors, formatting, and Clippy over the same Rust sources. Future external crate dependencies must keep Cargo and Bazel resolution aligned.

## 02.2 Commands

Run from the repository root:

```bash
bazel build //...
bazel test //...
bazel run //:agent-computer -- version --json
bazel run //:agent-computer -- capabilities --json
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Bazelisk reads `.bazelversion`; Rustup reads `rust-toolchain.toml`. Use an organization-provided HTTPS proxy or verified mirror if downloads are restricted. Do not change pinned versions to conceal a download failure.

CLI JSON is written to stdout and diagnostics to stderr. Unsupported commands or arguments exit with code 2. Runtime features are not delivered yet; a successful version query is not a service health check.

## 02.3 Development conventions

Each commit is a reviewable capability increment with its build targets, contract tests, and progress updates in both languages. Keep domain state separate from external side effects, then add persistence, adapters, and UI. Verification must exercise rejection paths and invariants involving real side effects.

Rule configuration follows the [official rules_rust documentation](https://bazelbuild.github.io/rules_rust/). A successful build only proves that current targets compile; real isolation and durability certification are tracked in [05 Verification](05-verification.md).
