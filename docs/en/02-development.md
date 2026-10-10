# 02. Development and builds

## 02.1 Toolchain

The baseline is Linux x86_64, Bazel 9.3.0, Rust 1.97.1 (edition 2024), and rules_rust 0.74.0. Versions are pinned in `.bazelversion`, `rust-toolchain.toml`, and `MODULE.bazel`; module resolution is recorded in `MODULE.bazel.lock`. The initial build downloads public rules and a Rust toolchain and needs a local C/C++ linker toolchain.

Bazel is the project build and test entry point, using `rust_library`, `rust_binary`, and `rust_test` directly. It does not invoke Cargo through a shell. The Cargo workspace supports editors, formatting, and Clippy over the same Rust sources. External crates use crate_universe over the same Cargo.toml/Cargo.lock. After dependency updates, run Bazel and commit Cargo.lock and MODULE.bazel.lock.

`REPO.bazel` excludes Cargo’s `target/` output from Bazel package discovery, preventing races with Cargo incremental directory cleanup. See [Bazel ignore_directories](https://bazel.build/rules/lib/globals/repo#ignore_directories).

The crate_universe resolver reuses the developer’s Cargo registry cache through the explicit `CARGO_BAZEL_ISOLATED=false` setting in `.bazelrc`. Cargo.lock still pins crate versions/checksums, and Bazel still runs the Rust compilation/test actions in its sandbox. CI should provide a controlled Cargo home/configuration. For a populated cache, `--repo_env=CARGO_NET_OFFLINE=true` permits dependency graph regeneration without remote index refresh; missing dependencies fail explicitly. After regeneration, verify `bazel build //... --lockfile_mode=error` against the committed module lock.

## 02.2 Commands

Run from the repository root:

```bash
bazel build //...
bazel test //...
bazel run //:agent-computer-server -- --help
bazel run //:agent-computer -- version --json
bazel run //:agent-computer -- capabilities --json
bazel run //:agent-computer -- validate examples/research.computer.yaml --json
bazel run //:agent-computer -- schema computer-set --json
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Service setup, credential administration, HTTP routes and OpenAPI are documented in [09 Control service](09-control-service.md).

The PostgreSQL integration suite requires locally installed server/client binaries and a non-root user; see [08 Persistence](08-persistence.md) for setup and Bazel environment flags. Tests fail if PostgreSQL is missing.

Bazelisk reads `.bazelversion`; Rustup reads `rust-toolchain.toml`. Use an organization-provided HTTPS proxy or verified mirror if downloads are restricted. Do not change pinned versions to conceal a download failure.

Remote CLI results are written to stdout and diagnostics to stderr. Unsupported commands or arguments exit with code 2. A successful local version query is not a service health check; remote commands use the separately configured service and workers.

## 02.3 Development conventions

Each commit is a reviewable capability increment with its build targets, contract tests, and progress updates in both languages. Keep domain state separate from external side effects, then add persistence, adapters, and UI. Verification must exercise rejection paths and invariants involving real side effects.

Rule configuration follows the [official rules_rust documentation](https://bazelbuild.github.io/rules_rust/). A successful build only proves that current targets compile; real isolation and durability certification are tracked in [05 Verification](05-verification.md).

The constrained Kubernetes library and explicit live component target are documented in [12 Kubernetes adapter](12-kubernetes-adapter.md).

The one-claim JuiceFS worker and explicit PostgreSQL/CSI target are documented in [13 Volume provisioning](13-volume-provisioning.md).

The authenticated [execution CLI](52-authenticated-execution-cli.md) supports the existing Computer, connection, writer and execution APIs, with explicit request JSON/idempotency keys and verified output downloads. External workflow/ecosystem adapters remain pending.
