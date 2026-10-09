# 03. Architecture and contracts

## 03.1 Implementation boundaries

| Path | Responsibility |
| --- | --- |
| `crates/core` | Domain contracts and state machines without infrastructure dependencies |
| `crates/definitions` | Bounded parsing, declaration semantics, fixed digests, structural schema |
| `crates/store` | PostgreSQL plans, definition grants, immutable SpecVersions, atomic retries/CAS, worker leases/receipts and event/Outbox transactions |
| `crates/server` | Axum HTTP routes, bounded requests, service authentication, plan/apply and local credential/grant/catalog administration |
| `crates/test-support` | Private real PostgreSQL clusters for integration tests only |
| `crates/cli` | Native CLI with version, capability status, declaration validation, and schema export |
| `docs/zh-CN`, `docs/en` | User and developer documentation with matching numbering |
| `codespec` | Authoritative detailed requirements, designs, decisions, and acceptance |
| `knowledge` | Glossary and CLI-managed navigation maps |

The Rust control service uses Tokio/Axum/SQLx/PostgreSQL, with trusted workers outside user Sandboxes. Browser Driver and ComputerView adapters retain the TypeScript/Playwright/React boundaries in the design. Core logic, control services, and CLI use Rust; Bazel manages the builds.

## 03.2 Required invariants

1. `revision` controls concurrent metadata updates; `generation` fences old runtime instances. They are independent.
2. Authentication establishes identity on the server. Connections, observation, modification, and GUI control have separate grants.
3. Lease expiry does not prove process termination. Handoff needs real draining or fencing evidence; otherwise recovery remains blocked.
4. Unknown outcomes cannot become successful through retries or automatic replay. Business acceptance belongs to an external authority.
5. Artifact objects are persisted before the manifest, current-pointer CAS, and Outbox are committed atomically.
6. Disconnecting, ending a task, or stopping a Computer never implicitly deletes persistent Workspaces/artifacts or cancels other people's active use.

## 03.3 Detailed specification index

The following authoritative specifications are currently in Chinese:

1. [Product requirements R01–R35](../../codespec/requirements/agent-computer.md)
2. [Technical design D01–D18](../../codespec/design/agent-computer.md)
3. [Deployment DP01–DP08](../../codespec/design/deployment-automation.md)
4. [Agentic contracts AR01–AR07](../../codespec/design/agentic-runtime-contracts.md)
5. [Ecosystem compositions E01–E10](../../codespec/design/ecosystem-integration.md)
6. [Acceptance T00–T43](../../codespec/test/agent-computer.md)
7. [Compute and storage decisions](../../codespec/decisions/compute-storage-selection.md)
8. [Multi-agent technical foundations](../../codespec/decisions/multi-agent-collaboration-foundations.md)
9. [Scenarios and gaps](../../codespec/requirements/agentic-scenarios-and-gaps.md)

In-memory or mock tests cannot replace multi-node isolation, durability, or real runtime acceptance.
