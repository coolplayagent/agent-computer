# 01. Overview

agent-computer provides connectable, interactive, persistent computer infrastructure for people and agents. A person can use a Computer independently of an agent or workflow. Computers have stable identities and replaceable compute instances; files, immutable artifacts, and checkpoints have separate lifecycles.

## 01.1 Current progress

Incremental implementation has started with a Rust workspace, Bazel builds, and CLI `version`, `capabilities`, `validate`, and `schema` commands. Computer lifecycle, lease handoff, Execution/Unknown, and idempotency domain rules now pass 28 contract tests; see [06 Domain contracts](06-domain-contracts.md). [07 Declaration validation](07-declarations.md) provides ComputerSet YAML/JSON static checks, fixed digests, and a JSON Schema. There is no running Computer service yet; the CLI reports runtime features as `unsupported`. Design baseline 0.5 describes the full target, not the currently available features.

## 01.2 First release scope

1. Equivalent single-node development and private multi-node Linux deployment; reference Kubernetes/containerd/gVisor compute and PostgreSQL/JuiceFS/S3 storage.
2. Browser actions, controlled process execution, files, fixed Artifacts, and application checkpoints.
3. Standalone and embedded ComputerView; many-to-many human/agent connections, one GUI controller, active-use protection, and fresh observations after handoff.
4. Presentations that run fixed generated application versions inside remote isolated environments.
5. Authorization, revocation, idempotency, fencing, unknown results, event replay, quotas, and deployment recovery.
6. Context bindings, action approval, environment handoff, purpose-limited evidence, and bounded admission.

The complete core target is [R01–R31 and R33](../../codespec/requirements/agent-computer.md) (detailed specification in Chinese). Optional team, development, and document compositions have separate R34–R35 delivery gates. R32 evaluation extensions require separate acceptance. Disabled extensions are not advertised as supported.

## 01.3 Reading order

Start with [02 Development](02-development.md), then [03 Architecture](03-architecture.md), [04 Implementation plan](04-implementation-plan.md), and [05 Verification](05-verification.md). A matching [Chinese edition](../zh-CN/01-overview.md) is available.
