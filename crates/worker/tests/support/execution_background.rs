//! Detach a real execution while retaining its original bounded writer slot.
use super::*;

pub async fn close(
    store: &Store,
    org: &OrganizationId,
    token: &str,
    lease: &WriterLease,
    queued: &ExecutionRequest,
    phase: &str,
) -> Value {
    let before = store
        .candidate_execution(token, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(before.lifetime, ExecutionLifetime::Background);
    assert_eq!(
        before.state,
        if phase == "before_dispatch" {
            ExecutionState::Queued
        } else {
            ExecutionState::Dispatching
        }
    );
    let closed = store
        .close_connection_session(token, &lease.connection_session_id)
        .await
        .unwrap();
    assert_eq!(closed.state, ConnectionState::Closed);
    assert!(closed.capabilities.is_empty());
    let current = store
        .reconcile_candidate_execution(org, &queued.execution_id)
        .await
        .unwrap();
    assert_eq!(current, before);
    let held = store
        .reconcile_candidate_writer(org, &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(held.state, WriterLeaseState::Held);
    assert_eq!(held.expires_at_ms, lease.expires_at_ms);
    assert_eq!(current.queue_deadline_at_ms, queued.queue_deadline_at_ms);
    json!({"phase":phase,"closed":closed,"execution_after_close":current,"writer_after_close":held,"deadline_unchanged":true})
}
