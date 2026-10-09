# 04. Incremental implementation plan

## 04.1 Delivery approach

Commit reviewable, buildable capability increments. A stage may span several commits, and its tests do not replace later acceptance gates. Domain development may proceed alongside deployment experiments; D1 combination certification remains a prerequisite for runtime releases.

## 04.2 Full scope and progress

| Stage | Deliverable | Requirements / acceptance | Status |
| --- | --- | --- | --- |
| 01 | Rust + Bazel, CLI foundation, numbered bilingual docs | R18 / T00 | Implemented; build, CLI, fmt/Clippy, and docs checked |
| 02 | Typed identities and Computer/Lease/Execution state constraints | R01, R08, R09, R21 / domain tests for D02, D04, D06, D08 | Implemented; 28 contract tests passed, runtime adapters pending |
| 03 | ComputerSet schema/validation, definition versions, plans, capabilities | R01, R14, R31 / T01, T17, T36 | Partial: static validation/digests/schema, immutable SpecVersions and authorized plan/apply delivered; runtime capability negotiation pending |
| 04 | PostgreSQL migrations, atomic idempotency, events/Outbox, CAS | R06, R08–R10 / T09, T12, T13 | Partial: registry/resource publication migrations, atomic retries/CAS, events/Outbox and replay implemented; durable coordination leases/receipts implemented; durable start admission/reservations/cancellation implemented; runtime preparation and external delivery workers pending |
| 05 | API/OpenAPI, service credentials/OIDC, grants, many-to-many connections | R02, R15, R22, R24, R33 / T02, T10, T18, T22, T38 | Partial: Axum service, OpenAPI subset, revocable scoped service credentials and protected validation/plan/apply with definition/reference grants and exact resource runtime grants delivered; bounded start admission delivered; OIDC, dispatch authorization and connections pending |
| 06 | Kubernetes/gVisor reconciliation, real fencing, start/stop/recovery | R02, R09, R13, R16 / T03, T08, T16, T19; B01–B07 | Partial: database coordination and constrained ephemeral Pod HTTPS adapter implemented; single-VM gVisor component probe passed; Computer runtime worker, Workspace mounts, physical fencing and full combination certification pending |
| 07 | JuiceFS files, Candidates, S3 Artifacts/Checkpoints, GC | R05, R06, R16 / T07, T09, T11, T19 | Partial: authorized first-revision Retain Volume worker, CSI binding and durable PVC/PV identities implemented; Candidate file/quota preparation and single-VM probes passed; committed initial input and authorized Candidate preparation worker implemented; product writer leases, Pod mounts, Artifacts and recovery pending |
| 08 | Controlled processes, bounded output, cancellation, lease watchdogs, Unknown reconciliation | R04, R08, R09 / T06, T08, T11, T12 | Pending |
| 09 | Browser Driver, visual/structured actions, profiles, control handoff | R03, R07, R19, R22 / T04, T05, T10, T24 | Pending |
| 10 | Standalone/embedded ComputerView, file editing, independent human use | R19, R21–R24 / T21–T24, T26 | Pending |
| 11 | Presentations, WebApplications, isolated trials, new-version saves | R20, R21 / T25–T27 | Pending |
| 12 | Helm, deployment CLI, operations UI, budgets/metering, backup/upgrade/recovery | R13, R16, R17, R25 / T16, T19, T20, T28–T30 | Pending |
| 13 | ContextBinding/delegation, ActionIntent/approval, Handoff, Evidence, admission | R26–R31, R33 / T31–T36, T38–T39 | Pending |
| 14 | Versioned workflow and independent tool adapters, recovery, examples | R11, R12, R14 / T14, T15, T17 | Pending |
| 15 | Full core fault matrix, multi-node runs, performance/cost, release manifest | R01–R31, R33 / T01–T36, T38–T39 | Pending acceptance |
| 16 | Optional teams, development/document compositions, end-to-end adapters | R34–R35 / T40–T43 | Pending; certify each composition separately |
| 17 | Optional isolated evaluation create/reset/step/verify/close | R32 / T37 | Later extension; currently unsupported |

## 04.3 Completion criteria

The full target remains the [product requirements](../../codespec/requirements/agent-computer.md), [D13 delivery sequence](../../codespec/design/agent-computer.md), and [acceptance gates](../../codespec/test/agent-computer.md). Pure-function tests establish local rules, not database durability, process termination, network isolation, or real browser/human workflows. Each runtime acceptance item needs a pinned commit, versions, environment, expected/observed results, and evidence. Blocked or unexecuted items remain visible.
