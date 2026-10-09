use super::*;

#[tokio::test]
async fn recorded_dispatch_never_reissues_and_cannot_claim_zero_dispatch_drain() {
    let (mut db, token, computer, input) = setup().await;
    let lease = acquire(&db, &token, &computer, &input).await;
    let permit = dispatch(&db, &token, &lease).await;
    assert_eq!(permit.prepared().data_inode, 123);
    assert_eq!(permit.lease().candidate_id, input.candidate_id);
    assert!(permit.lease().dispatch_recorded);
    assert_eq!(permit.dispatch_id(), "dispatch-one");
    assert_eq!(permit.input_digest(), format!("sha256:{}", "a".repeat(64)));
    db.crash_and_restart().await;
    assert!(matches!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &lease.lease_id,
                &command(permit.lease()),
                WriterDispatch {
                    dispatch_id: "second",
                    input_digest: permit.input_digest()
                }
            )
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
    let draining = db
        .store
        .release_candidate_writer(
            &token,
            &key("release"),
            &lease.lease_id,
            &command(permit.lease()),
        )
        .await
        .unwrap();
    assert_eq!(draining.state, WriterLeaseState::Draining);
    assert!(draining.release_proof.is_none());
    assert!(sqlx::query("INSERT INTO candidate_writer_drains (organization,lease_id,epoch,proof) SELECT organization,lease_id,epoch,'no_dispatch' FROM candidate_writer_leases").execute(&db.pool).await.is_err());
    assert!(
        sqlx::query("UPDATE candidate_writer_leases SET state='Released',revision=revision+1")
            .execute(&db.pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM candidate_writer_dispatches")
            .execute(&db.pool)
            .await
            .is_err()
    );
    db.store
        .close_connection_session(&token, &input.connection_session_id)
        .await
        .unwrap();
    let current = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(current.state, WriterLeaseState::Draining);
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
}

#[tokio::test]
async fn expiry_blocks_renewal_and_takeover_until_zero_dispatch_reconciliation() {
    let (db, token, computer, mut input) = setup().await;
    input.duration_seconds = 1;
    let lease = acquire(&db, &token, &computer, &input).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .state,
        WriterLeaseState::Draining
    );
    assert!(matches!(
        db.store
            .renew_candidate_writer(
                &token,
                &key("late"),
                &lease.lease_id,
                &RenewWriterLease {
                    lease: command(&lease),
                    duration_seconds: 30
                }
            )
            .await,
        Err(Error::WriterLeaseInactive)
    ));
    assert!(matches!(
        db.store
            .acquire_candidate_writer(&token, &key("takeover"), &computer, &input)
            .await,
        Err(Error::WriterLeaseBusy)
    ));
    let result = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(result.state, WriterLeaseState::Released);
    assert_eq!(result.release_proof.as_deref(), Some("no_dispatch"));
    let again = db
        .store
        .reconcile_candidate_writer(&org("acme"), &lease.lease_id)
        .await
        .unwrap();
    assert_eq!(again.revision, result.revision);
    assert_eq!(
        db.store
            .acquire_candidate_writer(&token, &key("takeover"), &computer, &input)
            .await
            .unwrap()
            .epoch,
        2
    );
}

#[tokio::test]
async fn outbox_failure_and_late_credential_expiry_roll_back_all_writer_records() {
    let (db, token, computer, input) = setup().await;
    sqlx::raw_sql("CREATE FUNCTION reject_writer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF EXISTS(SELECT 1 FROM events WHERE organization=NEW.organization AND sequence=NEW.sequence AND kind LIKE 'writer.%') THEN RAISE EXCEPTION 'injected'; END IF; RETURN NEW; END $$; CREATE TRIGGER reject_writer BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_writer();").execute(&db.pool).await.unwrap();
    let events = count(&db, "events").await;
    assert!(matches!(
        db.store
            .acquire_candidate_writer(&token, &key("acquire"), &computer, &input)
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "candidate_writer_leases").await, 0);
    assert_eq!(count(&db, "candidate_writer_epochs").await, 0);
    sqlx::query("DROP TRIGGER reject_writer ON outbox")
        .execute(&db.pool)
        .await
        .unwrap();
    let lease = acquire(&db, &token, &computer, &input).await;
    sqlx::raw_sql("CREATE TRIGGER reject_writer BEFORE INSERT ON outbox FOR EACH ROW EXECUTE FUNCTION reject_writer();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .release_candidate_writer(&token, &key("release"), &lease.lease_id, &command(&lease))
            .await,
        Err(Error::Database(_))
    ));
    assert!(matches!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &lease.lease_id,
                &command(&lease),
                WriterDispatch {
                    dispatch_id: "failed",
                    input_digest: &format!("sha256:{}", "a".repeat(64))
                }
            )
            .await,
        Err(Error::Database(_))
    ));
    assert_eq!(count(&db, "candidate_writer_drains").await, 0);
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(count(&db, "events").await, events + 1);
    sqlx::raw_sql("DROP TRIGGER reject_writer ON outbox; CREATE FUNCTION expire_writer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN UPDATE service_credentials SET expires_at=clock_timestamp() WHERE 'runtime.modify'=ANY(scopes); PERFORM pg_sleep(0.01); RETURN NEW; END $$; CREATE TRIGGER expire_writer AFTER INSERT ON candidate_writer_dispatches FOR EACH ROW EXECUTE FUNCTION expire_writer();").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .begin_candidate_writer_dispatch(
                &token,
                &lease.lease_id,
                &command(&lease),
                WriterDispatch {
                    dispatch_id: "late",
                    input_digest: &format!("sha256:{}", "a".repeat(64))
                }
            )
            .await,
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "candidate_writer_dispatches").await, 0);
    assert_eq!(
        db.store
            .candidate_writer(&token, &lease.lease_id)
            .await
            .unwrap()
            .revision,
        1
    );
}

#[tokio::test]
async fn migration_ten_does_not_infer_owners_and_readiness_detects_checksum_drift() {
    let (db, token, computer, input) = setup().await;
    sqlx::raw_sql("DROP TABLE candidate_writer_drains,candidate_writer_dispatches,candidate_writer_epochs,candidate_writer_leases; DROP FUNCTION guard_writer_record_insert(); DROP FUNCTION guard_writer_lease_mutation(); DELETE FROM _sqlx_migrations WHERE version=10;").execute(&db.pool).await.unwrap();
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    assert_eq!(count(&db, "candidate_writer_leases").await, 0);
    assert_eq!(count(&db, "connection_sessions").await, 1);
    acquire(&db, &token, &computer, &input).await;
    sqlx::query("UPDATE _sqlx_migrations SET checksum='\\x00'::bytea WHERE version=10")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
}
