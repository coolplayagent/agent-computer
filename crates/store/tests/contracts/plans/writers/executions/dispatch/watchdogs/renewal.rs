//! Synthetic SQL metadata only; no live guard or recovered execution capability.
use super::*;
use agent_computer_sandbox::{
    StartupGrant,
    renewal::{Challenge, Grant},
};
use agent_computer_watchdog::renewal::{Command, Receipt, request_digest};
use sqlx::{Postgres, Transaction};

struct Fixture {
    db: Database,
    attempt: ExecutionDispatchAttempt,
    arm: Value,
    startup: String,
}
impl Fixture {
    fn id(&self) -> &str {
        &self.attempt.intent().execution.execution_id
    }
    async fn deadline(&self) -> i64 {
        sqlx::query_scalar("SELECT execution_effective_deadline('acme',$1)")
            .bind(self.id())
            .fetch_one(&self.db.pool)
            .await
            .unwrap()
    }
    async fn writer(&self) -> (i64, i64) {
        sqlx::query_as("SELECT expires_at_ms,revision FROM candidate_writer_leases")
            .fetch_one(&self.db.pool)
            .await
            .unwrap()
    }
    async fn grant(
        &self,
        sequence: u32,
        previous_deadline: i64,
        previous_digest: &str,
    ) -> ExecutionRenewalGrant {
        let now: i64 =
            sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp())*1000)::bigint")
                .fetch_one(&self.db.pool)
                .await
                .unwrap();
        let challenge = Challenge {
            version: 1,
            startup_grant_digest: self.startup.clone(),
            sequence,
            nonce: format!("{sequence:064x}"),
        };
        let grant = Grant {
            version: 1,
            challenge_digest: challenge.digest().unwrap(),
            lease_budget_ms: 30000,
        };
        let digest = grant.digest().unwrap();
        let request = serde_json::from_value(self.arm["armed"]["request"].clone()).unwrap();
        let node_command = Command {
            version: 1,
            request_digest: request_digest(&request).unwrap(),
            sequence,
            grant_digest: digest.clone(),
            deadline_boottime_ms: 31000 + u64::from(sequence) * 1000,
        };
        let mut result = ExecutionRenewalGrant {
            sequence,
            challenge,
            grant,
            node_command,
            grant_digest: digest,
            previous_grant_digest: previous_digest.into(),
            renewal_digest: String::new(),
            granted_at_ms: now,
            previous_deadline_at_ms: previous_deadline,
            deadline_at_ms: now + 30000,
        };
        let d = self.attempt.intent();
        result.renewal_digest = typed_hash(
            "agent-computer/execution-renewal-authorization-v1",
            &(
                &d.organization,
                &d.execution.execution_id,
                &d.intent_digest,
                result.sequence,
                &result.challenge,
                &result.grant,
                &result.node_command,
                &result.grant_digest,
                &result.previous_grant_digest,
                result.granted_at_ms,
                result.previous_deadline_at_ms,
                result.deadline_at_ms,
            ),
        );
        result
    }
    fn proof(&self, grant: &ExecutionRenewalGrant) -> [Receipt; 2] {
        ["armed", "backup_armed"].map(|name| Receipt {
            version: 1,
            event: "renewed".into(),
            journal: serde_json::from_value(self.arm[name]["journal"].clone()).unwrap(),
            command: grant.node_command.clone(),
            previous_deadline_boottime_ms: if grant.sequence == 1 {
                30000
            } else {
                30000 + u64::from(grant.sequence) * 1000
            },
            accepted_boottime_ms: 1000 + u64::from(grant.sequence) * 1000,
        })
    }
    async fn insert(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        grant: &ExecutionRenewalGrant,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        sqlx::query("INSERT INTO execution_renewal_grants (organization,execution_id,sequence,challenge,grant_body,node_command,grant_digest,previous_grant_digest,renewal_digest,granted_at_ms,previous_deadline_at_ms,deadline_at_ms) VALUES ('acme',$1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)")
            .bind(self.id()).bind(grant.sequence as i32).bind(serde_json::to_value(&grant.challenge).unwrap()).bind(serde_json::to_value(&grant.grant).unwrap())
            .bind(serde_json::to_value(&grant.node_command).unwrap()).bind(&grant.grant_digest).bind(&grant.previous_grant_digest).bind(&grant.renewal_digest)
            .bind(grant.granted_at_ms).bind(grant.previous_deadline_at_ms).bind(grant.deadline_at_ms).execute(&mut **tx).await
    }
    async fn ack(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        grant: &ExecutionRenewalGrant,
        evidence: &Value,
    ) -> Result<sqlx::postgres::PgQueryResult, sqlx::Error> {
        let now: i64 =
            sqlx::query_scalar("SELECT floor(extract(epoch from clock_timestamp())*1000)::bigint")
                .fetch_one(&mut **tx)
                .await
                .unwrap();
        // Canonicalize object keys, as the trusted Store reader does.
        let proof: [Receipt; 2] = serde_json::from_value(evidence.clone()).unwrap();
        let d = self.attempt.intent();
        let digest = typed_hash(
            "agent-computer/execution-renewal-ack-v1",
            &(
                &d.organization,
                &d.execution.execution_id,
                &grant.renewal_digest,
                &proof,
                now,
            ),
        );
        sqlx::query("INSERT INTO execution_renewal_acks (organization,execution_id,sequence,evidence,evidence_digest,acknowledged_at_ms) VALUES ('acme',$1,$2,$3,$4,$5)")
            .bind(self.id()).bind(grant.sequence as i32).bind(evidence).bind(digest).bind(now).execute(&mut **tx).await
    }
    async fn extend(&self, tx: &mut Transaction<'_, Postgres>, deadline: i64) {
        sqlx::query("UPDATE candidate_writer_leases SET expires_at_ms=GREATEST(expires_at_ms,$1),revision=revision+1")
            .bind(deadline).execute(&mut **tx).await.unwrap();
    }
}
fn typed_hash(domain: &str, value: &impl serde::Serialize) -> String {
    let mut h = Sha256::new();
    h.update(domain);
    h.update([0]);
    h.update(serde_json::to_vec(&serde_json::to_value(value).unwrap()).unwrap());
    format!("sha256:{:x}", h.finalize())
}
async fn fixture(expires_ms: i64) -> Fixture {
    let (db, attempt, plan, arm, now, _) = fixture_with_policy(false, true).await;
    insert(&db, &attempt, &plan, &arm, now, now + expires_ms)
        .await
        .unwrap();
    let challenge = pods::challenge(&attempt);
    let grant = StartupGrant {
        version: 2,
        challenge_digest: challenge.digest().unwrap(),
        lease_budget_ms: 1,
        hard_budget_ms: Some(
            (attempt.intent().hard_deadline_at_ms.unwrap() - attempt.intent().deadline_at_ms + 1)
                as u32,
        ),
    };
    let startup = grant.digest().unwrap();
    sqlx::query("INSERT INTO execution_startup_grants (organization,execution_id,pod_uid,challenge,grant_body,grant_digest,granted_at_ms) VALUES ('acme',$1,'pod-one',$2,$3,$4,$5)")
        .bind(&attempt.intent().execution.execution_id).bind(serde_json::to_value(challenge).unwrap()).bind(serde_json::to_value(grant).unwrap()).bind(&startup).bind(now).execute(&db.pool).await.unwrap();
    Fixture {
        db,
        attempt,
        arm,
        startup,
    }
}

#[tokio::test]
async fn authorization_alone_never_extends_and_ack_requires_atomic_writer_update() {
    let mut f = fixture(5000).await;
    let original = f.deadline().await;
    let writer = f.writer().await;
    let grant = f.grant(1, original, &f.startup).await;
    let mut tx = f.db.pool.begin().await.unwrap();
    f.insert(&mut tx, &grant).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(f.deadline().await, original);
    assert_eq!(f.writer().await, writer);
    let proof = serde_json::to_value(f.proof(&grant)).unwrap();
    let mut tx = f.db.pool.begin().await.unwrap();
    f.ack(&mut tx, &grant, &proof).await.unwrap();
    assert!(
        tx.commit()
            .await
            .unwrap_err()
            .to_string()
            .contains("writer update incomplete")
    );
    assert_eq!(count(&f.db, "execution_renewal_acks").await, 0);
    let mut tx = f.db.pool.begin().await.unwrap();
    f.ack(&mut tx, &grant, &proof).await.unwrap();
    f.extend(&mut tx, grant.deadline_at_ms).await;
    tx.commit().await.unwrap();
    assert_eq!(f.deadline().await, grant.deadline_at_ms);
    assert_eq!(f.writer().await, (grant.deadline_at_ms, writer.1 + 1));
    for table in ["execution_renewal_grants", "execution_renewal_acks"] {
        for sql in [
            format!("UPDATE {table} SET sequence=sequence+1"),
            format!("DELETE FROM {table}"),
        ] {
            assert!(sqlx::query(&sql).execute(&f.db.pool).await.is_err());
        }
    }
    let progress: Value = sqlx::query_scalar("SELECT execution_renewal_progress('acme',$1)")
        .bind(f.id())
        .fetch_one(&f.db.pool)
        .await
        .unwrap();
    assert_eq!(
        progress,
        json!({"sequence":1,"grant_digest":grant.grant_digest})
    );
    // Historical metadata survives WAL; it cannot issue a replacement dispatch.
    f.db.crash_and_restart().await;
    assert_eq!(f.deadline().await, grant.deadline_at_ms);
    assert_eq!(
        f.db.store
            .reconcile_candidate_execution(&org("acme"), f.id())
            .await
            .unwrap()
            .state,
        ExecutionState::Dispatching
    );
    assert!(matches!(
        f.db.store
            .begin_candidate_execution_dispatch(&org("acme"), f.id(), 2)
            .await,
        Err(Error::DispatchAlreadyStarted)
    ));
}

#[tokio::test]
async fn renewal_rejects_skips_wrong_startup_and_partial_or_transplanted_proofs() {
    let f = fixture(5000).await;
    let original = f.deadline().await;
    let grant = f.grant(1, original, &f.startup).await;
    for change in 0..5 {
        let mut bad = grant.clone();
        match change {
            0 => bad.sequence = 2,
            1 => bad.challenge.startup_grant_digest = format!("sha256:{}", "f".repeat(64)),
            2 => bad.previous_deadline_at_ms += 1,
            3 => bad.deadline_at_ms = f.attempt.intent().hard_deadline_at_ms.unwrap() + 1,
            _ => bad.previous_grant_digest = format!("sha256:{}", "f".repeat(64)),
        }
        let mut tx = f.db.pool.begin().await.unwrap();
        assert!(f.insert(&mut tx, &bad).await.is_err(), "change {change}");
        tx.rollback().await.unwrap();
    }
    let mut tx = f.db.pool.begin().await.unwrap();
    f.insert(&mut tx, &grant).await.unwrap();
    tx.commit().await.unwrap();
    let proof = serde_json::to_value(f.proof(&grant)).unwrap();
    for (pointer, value) in [
        ("/0/journal/id", json!("foreign")),
        ("/1/command/sequence", json!(2)),
        (
            "/1/command/grant_digest",
            json!(format!("sha256:{}", "f".repeat(64))),
        ),
        ("/0/previous_deadline_boottime_ms", json!(29999)),
        ("/1/accepted_boottime_ms", json!(30000)),
    ] {
        let mut bad = proof.clone();
        *bad.pointer_mut(pointer).unwrap() = value;
        let mut tx = f.db.pool.begin().await.unwrap();
        assert!(f.ack(&mut tx, &grant, &bad).await.is_err(), "{pointer}");
        tx.rollback().await.unwrap();
    }
    assert_eq!(f.deadline().await, original);
    assert_eq!(count(&f.db, "execution_renewal_acks").await, 0);
    let mut tx = f.db.pool.begin().await.unwrap();
    f.ack(&mut tx, &grant, &proof).await.unwrap();
    f.extend(&mut tx, grant.deadline_at_ms).await;
    tx.commit().await.unwrap();
    let next = f.grant(2, grant.deadline_at_ms, &grant.grant_digest).await;
    let mut tx = f.db.pool.begin().await.unwrap();
    f.insert(&mut tx, &next).await.unwrap();
    f.ack(
        &mut tx,
        &next,
        &serde_json::to_value(f.proof(&next)).unwrap(),
    )
    .await
    .unwrap();
    f.extend(&mut tx, next.deadline_at_ms).await;
    tx.commit().await.unwrap();
    assert_eq!(f.deadline().await, next.deadline_at_ms);
}

#[tokio::test]
async fn renewal_commit_crossing_old_expiry_rolls_back_even_with_new_writer_deadline() {
    for ack in [false, true] {
        let f = fixture(1500).await;
        let original = f.deadline().await;
        let writer = f.writer().await;
        let grant = f.grant(1, original, &f.startup).await;
        let mut tx = f.db.pool.begin().await.unwrap();
        f.insert(&mut tx, &grant).await.unwrap();
        if ack {
            tx.commit().await.unwrap();
            tx = f.db.pool.begin().await.unwrap();
            f.ack(
                &mut tx,
                &grant,
                &serde_json::to_value(f.proof(&grant)).unwrap(),
            )
            .await
            .unwrap();
            f.extend(&mut tx, grant.deadline_at_ms).await;
        }
        sqlx::query("SELECT pg_sleep(GREATEST(0,($1-floor(extract(epoch from clock_timestamp())*1000))/1000.0)+0.1)")
            .bind(original).execute(&mut *tx).await.unwrap();
        assert!(
            tx.commit()
                .await
                .unwrap_err()
                .to_string()
                .contains("original deadline elapsed")
        );
        assert_eq!(count(&f.db, "execution_renewal_acks").await, 0);
        assert_eq!(f.writer().await, writer);
        assert_eq!(f.deadline().await, original);
        let mut tx = f.db.pool.begin().await.unwrap();
        assert!(f.insert(&mut tx, &grant).await.is_err());
    }
}
