use super::*;
use agent_computer_store::{Error, reconciliation::*};

async fn publish(
    db: &Database,
    token: &str,
    name: &str,
    document: &Value,
) -> (DefinitionPlan, DefinitionOperation) {
    let plan = db
        .store
        .create_definition_plan(token, &key(&format!("{name}-plan")), &checked(document))
        .await
        .unwrap();
    let operation = db
        .store
        .apply_definition_plan(
            token,
            &key(&format!("{name}-apply")),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    (plan, operation)
}
async fn claim(db: &Database, worker: &str, seconds: u64) -> ReconcileLease {
    match db
        .store
        .claim_reconciliation(
            &org("acme"),
            &WorkerId::new(worker).unwrap(),
            Duration::from_secs(seconds),
        )
        .await
        .unwrap()
    {
        ClaimOutcome::Claimed(lease) => *lease,
        other => panic!("expected lease, got {other:?}"),
    }
}
async fn idle(db: &Database) {
    assert!(matches!(
        db.store
            .claim_reconciliation(
                &org("acme"),
                &WorkerId::new("probe").unwrap(),
                Duration::from_secs(30)
            )
            .await
            .unwrap(),
        ClaimOutcome::Idle
    ));
}
// Synthetic adapter evidence tests only the control-store binding. No external
// backend or physical fencing is claimed by this fixture.
fn receipt(lease: &ReconcileLease) -> EffectReceipt {
    EffectReceipt {
        step_id: lease.task().step_id.clone(),
        resource_id: lease.task().resource_id.clone(),
        revision: lease.task().revision,
        spec_digest: lease.task().spec_digest.clone(),
        backend: "fixture".into(),
        object_uid: "fixture-uid".into(),
        evidence_id: "fixture-evidence".into(),
    }
}
async fn succeed(db: &Database, lease: &ReconcileLease) -> IntentProgress {
    let permit = db.store.begin_reconciliation_dispatch(lease).await.unwrap();
    assert_eq!(permit.task().step_id, lease.task().step_id);
    assert_eq!(permit.epoch(), lease.epoch());
    db.store
        .finish_reconciliation(
            lease,
            ReconcileOutcome::Applied {
                receipt: receipt(lease),
            },
        )
        .await
        .unwrap()
}
fn agent_document(name: &str, agent: &str) -> Value {
    serde_json::json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":name},"spec":{"agents":[{"name":agent,"mode":"external","adapter":"tools-api"}]}})
}

mod admission;
mod claims;
mod leases;
mod objects;
mod results;
