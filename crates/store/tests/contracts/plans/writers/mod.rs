use super::{
    runtime::{allow, runtime_token},
    *,
};
use agent_computer_store::{
    Error,
    runtime::{connections::*, writers::*, *},
};

mod authority;
mod executions;
mod reads;
mod recovery;
mod stops;

async fn connection(
    db: &Database,
    token: &str,
    computer: &str,
    name: &str,
    seconds: u32,
) -> ConnectionSession {
    db.store
        .create_connection_session(
            token,
            &key(name),
            computer,
            &ConnectRequest {
                requested_capabilities: vec![
                    RuntimePermission::Connect,
                    RuntimePermission::Read,
                    RuntimePermission::Modify,
                ],
                lifetime_seconds: seconds,
            },
        )
        .await
        .unwrap()
}
async fn setup() -> (Database, String, String, AcquireWriterLease) {
    let (db, token, computer, start, target) = super::preparation::setup().await;
    let preparation = super::preparation::claim(&db, &start, &target, "preparer").await;
    db.store
        .begin_candidate_preparation(&preparation)
        .await
        .unwrap();
    // Synthetic adapter evidence: this suite proves database authority only.
    db.store
        .finish_candidate_preparation(&preparation, &super::preparation::evidence(&preparation))
        .await
        .unwrap();
    for permission in [RuntimePermission::Connect, RuntimePermission::Modify] {
        allow(&db, &computer, RuntimeKind::Computer, permission, None).await;
    }
    let session = connection(&db, &token, &computer, "connect", 900).await;
    let input = AcquireWriterLease {
        scope: WriterScope::Modify,
        connection_session_id: session.session_id,
        candidate_id: start.candidate_id,
        generation: start.generation,
        duration_seconds: 30,
    };
    (db, token, computer, input)
}
fn command(lease: &WriterLease) -> WriterLeaseCommand {
    WriterLeaseCommand {
        connection_session_id: lease.connection_session_id.clone(),
        generation: lease.generation,
        epoch: lease.epoch,
        expected_revision: lease.revision,
    }
}
async fn acquire(
    db: &Database,
    token: &str,
    computer: &str,
    input: &AcquireWriterLease,
) -> WriterLease {
    db.store
        .acquire_candidate_writer(token, &key("acquire"), computer, input)
        .await
        .unwrap()
}
async fn dispatch(db: &Database, token: &str, lease: &WriterLease) -> WriterDispatchPermit {
    db.store
        .begin_candidate_writer_dispatch(
            token,
            &lease.lease_id,
            &command(lease),
            WriterDispatch {
                dispatch_id: "dispatch-one",
                input_digest: &format!("sha256:{}", "a".repeat(64)),
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn acquisition_survives_wal_restart_and_does_not_advertise_ready() {
    let (mut db, token, computer, input) = setup().await;
    let first = acquire(&db, &token, &computer, &input).await;
    assert_eq!(
        (first.epoch, first.revision, first.state),
        (1, 1, WriterLeaseState::Held)
    );
    assert!(first.expires_at_ms - first.checked_at_ms <= 30_000);
    db.crash_and_restart().await;
    let before = count(&db, "events").await;
    let replay = acquire(&db, &token, &computer, &input).await;
    assert_eq!(
        (replay.lease_id, replay.expires_at_ms, replay.revision),
        (first.lease_id, first.expires_at_ms, first.revision)
    );
    assert_eq!(count(&db, "events").await, before);
    assert_eq!(count(&db, "candidate_writer_epochs").await, 1);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    let runtime = db.store.computer_runtime(&token, &computer).await.unwrap();
    assert_eq!(runtime.start_state, Some(StartState::Prepared));
    assert!(!runtime.ready);
}

#[tokio::test]
async fn competing_connections_and_stale_commands_cannot_change_owner() {
    let (db, token, computer, one) = setup().await;
    let session = connection(&db, &token, &computer, "two", 900).await;
    let two = AcquireWriterLease {
        connection_session_id: session.session_id,
        ..one.clone()
    };
    let a = key("race-a");
    let b = key("race-b");
    let (a, b) = tokio::join!(
        db.store
            .acquire_candidate_writer(&token, &a, &computer, &one),
        db.store
            .acquire_candidate_writer(&token, &b, &computer, &two)
    );
    let lease = match (a, b) {
        (Ok(v), Err(Error::WriterLeaseBusy)) | (Err(Error::WriterLeaseBusy), Ok(v)) => v,
        other => panic!("{other:?}"),
    };
    for changed in [
        WriterLeaseCommand {
            generation: lease.generation + 1,
            ..command(&lease)
        },
        WriterLeaseCommand {
            epoch: lease.epoch + 1,
            ..command(&lease)
        },
        WriterLeaseCommand {
            expected_revision: lease.revision + 1,
            ..command(&lease)
        },
        WriterLeaseCommand {
            connection_session_id: "other-session".into(),
            ..command(&lease)
        },
    ] {
        assert!(matches!(
            db.store
                .renew_candidate_writer(
                    &token,
                    &key("stale"),
                    &lease.lease_id,
                    &RenewWriterLease {
                        lease: changed,
                        duration_seconds: 30
                    }
                )
                .await,
            Err(Error::WriterLeaseConflict)
        ));
    }
    assert_eq!(count(&db, "candidate_writer_leases").await, 1);
    assert_eq!(count(&db, "candidate_writer_epochs").await, 1);
}

#[tokio::test]
async fn renewal_retry_never_extends_twice_and_session_expiry_caps_authority() {
    let (db, token, computer, mut input) = setup().await;
    let session = connection(&db, &token, &computer, "short", 5).await;
    input.connection_session_id = session.session_id;
    let first = acquire(&db, &token, &computer, &input).await;
    assert_eq!(first.expires_at_ms, session.expires_at_ms);
    let renew = RenewWriterLease {
        lease: command(&first),
        duration_seconds: 30,
    };
    let next = db
        .store
        .renew_candidate_writer(&token, &key("renew"), &first.lease_id, &renew)
        .await
        .unwrap();
    assert_eq!(
        (next.revision, next.expires_at_ms),
        (2, session.expires_at_ms)
    );
    let events = count(&db, "events").await;
    let again = db
        .store
        .renew_candidate_writer(&token, &key("renew"), &first.lease_id, &renew)
        .await
        .unwrap();
    assert_eq!(
        (again.revision, again.expires_at_ms),
        (next.revision, next.expires_at_ms)
    );
    assert_eq!(count(&db, "events").await, events);
    for seconds in [0, 31, u32::MAX] {
        assert!(matches!(
            db.store
                .renew_candidate_writer(
                    &token,
                    &key("invalid"),
                    &first.lease_id,
                    &RenewWriterLease {
                        lease: command(&next),
                        duration_seconds: seconds
                    }
                )
                .await,
            Err(Error::InvalidRuntimeRequest)
        ));
    }
}

#[tokio::test]
async fn zero_dispatch_release_is_durable_and_handoff_advances_epoch() {
    let (db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let released = db
        .store
        .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&lease))
        .await
        .unwrap();
    assert_eq!(released.state, WriterLeaseState::Released);
    assert_eq!(released.release_proof.as_deref(), Some("no_dispatch"));
    let before = count(&db, "events").await;
    db.store
        .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&lease))
        .await
        .unwrap();
    assert_eq!(count(&db, "events").await, before);
    let next = db
        .store
        .acquire_candidate_writer(&token, &key("next-epoch"), &computer, &input)
        .await
        .unwrap();
    assert_eq!(next.lease_id, lease.lease_id);
    assert_eq!(next.epoch, lease.epoch + 1);
    assert!(next.release_proof.is_none());
    for result in [
        db.store
            .acquire_candidate_writer(&token, &key("acquire"), &computer, &input)
            .await,
        db.store
            .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&lease))
            .await,
    ] {
        assert!(matches!(result, Err(Error::WriterLeaseConflict)));
    }
    assert_eq!(count(&db, "candidate_writer_epochs").await, 2);
    assert_eq!(count(&db, "candidate_writer_drains").await, 1);
    assert!(
        sqlx::query("DELETE FROM candidate_writer_drains")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE candidate_writer_epochs SET session_id='other'")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert_eq!(
        db.store
            .computer_runtime(&token, &computer)
            .await
            .unwrap()
            .start_state,
        Some(StartState::Prepared)
    );
}
