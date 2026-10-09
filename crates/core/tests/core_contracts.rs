//! Contract examples derived from D02/D04/D06/D08, not runtime acceptance T01–T43.

use agent_computer_core::Error;
use agent_computer_core::computer::*;
use agent_computer_core::execution::*;
use agent_computer_core::idempotency::*;
use agent_computer_core::identity::*;
use agent_computer_core::lease::*;

fn generation(n: u64) -> Generation {
    Generation::from_u64(n)
}
fn evidence(name: &str) -> EvidenceId {
    EvidenceId::new(name).unwrap()
}
fn digest(byte: char) -> InputDigest {
    InputDigest::parse(&format!("sha256:{}", byte.to_string().repeat(64))).unwrap()
}
fn computer() -> Computer {
    Computer::new(
        OrganizationId::new("org_1").unwrap(),
        ComputerId::new("cmp_1").unwrap(),
        WorkspaceId::new("ws_1").unwrap(),
    )
}
fn ready_computer() -> Computer {
    let mut c = computer();
    let g = c.start(c.revision()).unwrap();
    c.ready(c.revision(), g).unwrap();
    c
}
fn fence(c: &Computer) -> RuntimeFence {
    RuntimeFence {
        computer_id: c.id().clone(),
        generation: c.generation(),
        evidence_id: evidence("fence_1"),
    }
}
fn checkpoint() -> CheckpointOutcome {
    CheckpointOutcome::Committed(CheckpointId::new("cp_1").unwrap())
}
fn owner(name: &str) -> ConnectionId {
    ConnectionId::new(name).unwrap()
}
fn lease(name: &str) -> Lease {
    Lease::new(
        OrganizationId::new("org_1").unwrap(),
        LeaseId::new(name).unwrap(),
        ComputerId::new("cmp_1").unwrap(),
        generation(1),
        LeaseScope::Gui(AppId::new("browser_1").unwrap()),
    )
    .unwrap()
}
fn execution() -> Execution {
    Execution::new(
        OrganizationId::new("org_1").unwrap(),
        ExecutionId::new("exec_1").unwrap(),
        ComputerId::new("cmp_1").unwrap(),
        generation(1),
        digest('a'),
    )
    .unwrap()
}
fn receipt(outcome: Outcome) -> Receipt {
    Receipt {
        outcome,
        evidence_id: evidence("receipt_1"),
    }
}

#[path = "contracts/computer.rs"]
mod computer_contracts;

#[path = "contracts/lease.rs"]
mod lease_contracts;

#[path = "contracts/execution.rs"]
mod execution_contracts;

#[path = "contracts/idempotency.rs"]
mod idempotency_contracts;

#[path = "contracts/identity.rs"]
mod identity_contracts;

fn request() -> RequestIdentity {
    RequestIdentity {
        organization: OrganizationId::new("org_1").unwrap(),
        principal: PrincipalId::new("human_1").unwrap(),
        operation: OperationKind::new("process").unwrap(),
        key: IdempotencyKey::new("submit_1").unwrap(),
        input: digest('a'),
    }
}
