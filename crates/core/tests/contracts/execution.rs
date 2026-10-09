use super::*;

#[test]
fn queued_cancel_prevents_any_dispatch() {
    let mut e = execution();
    e.request_cancel(e.revision()).unwrap();
    assert_eq!(e.status(), ExecutionStatus::Cancelled);
    assert!(e.cancel_requested());
    assert_eq!(
        e.dispatch(e.revision(), generation(1)),
        Err(Error::InvalidTransition)
    );
}

#[test]
fn running_cancel_does_not_claim_that_the_process_has_stopped() {
    let mut e = execution();
    e.dispatch(e.revision(), generation(1)).unwrap();
    e.request_cancel(e.revision()).unwrap();
    assert_eq!(e.status(), ExecutionStatus::Running);
    assert!(e.receipt().is_none());
    let snapshot = e.clone();
    e.request_cancel(e.revision()).unwrap();
    assert_eq!(e, snapshot);
    e.finish(e.revision(), generation(1), receipt(Outcome::Cancelled))
        .unwrap();
    assert_eq!(e.status(), ExecutionStatus::Cancelled);
    assert_eq!(e.receipt().unwrap().evidence_id.as_str(), "receipt_1");
}

#[test]
fn actual_success_can_win_the_cancel_race_without_fabricating_cancellation() {
    let mut e = execution();
    e.dispatch(e.revision(), generation(1)).unwrap();
    e.request_cancel(e.revision()).unwrap();
    e.finish(e.revision(), generation(1), receipt(Outcome::Succeeded))
        .unwrap();
    assert_eq!(e.status(), ExecutionStatus::Succeeded);
    assert!(e.cancel_requested());
}

#[test]
fn stale_workers_cannot_dispatch_or_complete_an_execution() {
    let mut e = execution();
    let before = e.clone();
    assert_eq!(
        e.dispatch(e.revision(), generation(2)),
        Err(Error::StaleGeneration)
    );
    assert_eq!(e, before);
    e.dispatch(e.revision(), generation(1)).unwrap();
    let before = e.clone();
    assert_eq!(
        e.finish(e.revision(), generation(2), receipt(Outcome::Succeeded)),
        Err(Error::StaleGeneration)
    );
    assert_eq!(e, before);
}

#[test]
fn unknown_outcomes_cannot_be_replayed_or_overwritten_by_late_receipts() {
    let mut e = execution();
    e.dispatch(e.revision(), generation(1)).unwrap();
    e.mark_unknown(e.revision(), evidence("lost_receipt"))
        .unwrap();
    let before = e.clone();
    assert_eq!(
        e.dispatch(e.revision(), generation(1)),
        Err(Error::InvalidTransition)
    );
    assert_eq!(
        e.finish(e.revision(), generation(1), receipt(Outcome::Succeeded)),
        Err(Error::InvalidTransition)
    );
    assert_eq!(e, before);
    e.request_cancel(e.revision()).unwrap();
    assert_eq!(e.status(), ExecutionStatus::Unknown);
    e.reconcile(e.revision(), receipt(Outcome::Succeeded))
        .unwrap();
    assert_eq!(e.status(), ExecutionStatus::Unknown);
    assert_eq!(e.unknown_evidence().unwrap().as_str(), "lost_receipt");
    assert_eq!(e.resolution().unwrap().outcome, Outcome::Succeeded);
    assert!(e.receipt().is_none());
    let resolved = e.clone();
    assert_eq!(
        e.reconcile(e.revision(), receipt(Outcome::Failed)),
        Err(Error::InvalidTransition)
    );
    assert_eq!(e, resolved);
}

#[test]
fn terminal_receipts_are_immutable_and_failed_is_not_succeeded() {
    for (outcome, status) in [
        (Outcome::Succeeded, ExecutionStatus::Succeeded),
        (Outcome::Failed, ExecutionStatus::Failed),
        (Outcome::Cancelled, ExecutionStatus::Cancelled),
    ] {
        let mut e = execution();
        e.dispatch(e.revision(), generation(1)).unwrap();
        e.finish(e.revision(), generation(1), receipt(outcome))
            .unwrap();
        assert_eq!(e.status(), status);
        let before = e.clone();
        assert_eq!(
            e.finish(e.revision(), generation(1), receipt(Outcome::Succeeded)),
            Err(Error::InvalidTransition)
        );
        assert_eq!(
            e.mark_unknown(e.revision(), evidence("late")),
            Err(Error::InvalidTransition)
        );
        assert_eq!(
            e.request_cancel(e.revision()),
            Err(Error::InvalidTransition)
        );
        assert_eq!(e, before);
    }
}

#[test]
fn stale_revision_cannot_complete_or_reconcile_execution() {
    let mut e = execution();
    e.dispatch(e.revision(), generation(1)).unwrap();
    let before = e.clone();
    assert_eq!(
        e.finish(
            Revision::INITIAL,
            generation(1),
            receipt(Outcome::Succeeded)
        ),
        Err(Error::RevisionConflict)
    );
    assert_eq!(
        e.request_cancel(Revision::INITIAL),
        Err(Error::RevisionConflict)
    );
    assert_eq!(e, before);
    e.mark_unknown(e.revision(), evidence("lost")).unwrap();
    let before = e.clone();
    assert_eq!(
        e.reconcile(Revision::INITIAL, receipt(Outcome::Succeeded)),
        Err(Error::RevisionConflict)
    );
    assert_eq!(e, before);
}
