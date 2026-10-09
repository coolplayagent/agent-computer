use super::*;

#[test]
fn retries_return_original_execution_but_changed_intent_conflicts() {
    let original = request();
    let e = execution();
    let record = IdempotencyRecord::new(original.clone(), e.id().clone());
    assert_eq!(record.retry(&original), Ok(e.id()));
    let mut changed = original.clone();
    changed.input = digest('b');
    assert_eq!(record.retry(&changed), Err(Error::IdempotencyConflict));
    assert_eq!(record.retry(&original), Ok(e.id()));
}

#[test]
fn idempotency_never_crosses_organization_principal_operation_or_key() {
    let original = request();
    let record = IdempotencyRecord::new(original.clone(), execution().id().clone());
    for field in 0..4 {
        let mut changed = original.clone();
        match field {
            0 => changed.organization = OrganizationId::new("org_other").unwrap(),
            1 => changed.principal = PrincipalId::new("agent_other").unwrap(),
            2 => changed.operation = OperationKind::new("browser_act").unwrap(),
            _ => changed.key = IdempotencyKey::new("submit_other").unwrap(),
        }
        assert_eq!(record.retry(&changed), Err(Error::IdempotencyScopeMismatch));
    }
}

#[test]
fn retired_keys_remain_tombstones_instead_of_becoming_new_work() {
    let original = request();
    let mut record = IdempotencyRecord::new(original.clone(), execution().id().clone());
    record.retire();
    assert_eq!(record.retry(&original), Err(Error::IdempotencyGone));
    let mut changed = original;
    changed.input = digest('b');
    assert_eq!(record.retry(&changed), Err(Error::IdempotencyGone));
}
