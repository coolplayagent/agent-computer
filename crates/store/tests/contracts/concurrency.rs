use crate::support::*;
use agent_computer_core::identity::Revision;
use agent_computer_store::{Error, Precondition};
use std::time::Duration;
use tokio::task::JoinSet;

#[tokio::test]
async fn simultaneous_retries_and_cas_have_one_winner() {
    let db = Database::new().await;
    let mut tasks = JoinSet::new();
    for _ in 0..12 {
        let store = db.store.clone();
        tasks.spawn(async move {
            record(
                &store,
                "acme",
                "alice",
                "same",
                "research",
                Precondition::Create,
            )
            .await
        });
    }
    while let Some(result) = tasks.join_next().await {
        let receipt = result.unwrap().unwrap();
        assert_eq!((receipt.revision, receipt.event_sequence), (1, 1));
    }
    for index in 0..12 {
        let store = db.store.clone();
        tasks.spawn(async move {
            record(
                &store,
                "acme",
                "alice",
                &format!("cas-{index}"),
                "research",
                Precondition::Match(Revision::INITIAL),
            )
            .await
        });
    }
    let mut successes = 0;
    while let Some(result) = tasks.join_next().await {
        match result.unwrap() {
            Ok(_) => successes += 1,
            Err(Error::RevisionConflict) => {}
            other => panic!("unexpected CAS result: {other:?}"),
        }
    }
    assert_eq!(successes, 1);
    assert_eq!(db.store.snapshot(&org("acme")).await.unwrap().watermark, 2);
    assert_eq!(
        db.store
            .replay(&org("acme"), 0, 100)
            .await
            .unwrap()
            .events
            .len(),
        2
    );
}

#[tokio::test]
async fn organization_lock_orders_commits_without_blocking_other_organizations() {
    let db = Database::new().await;
    record(
        &db.store,
        "acme",
        "alice",
        "first",
        "first",
        Precondition::Create,
    )
    .await
    .unwrap();
    let mut held = db.pool.begin().await.unwrap();
    sqlx::query(
        "SELECT last_sequence FROM organization_streams WHERE organization='acme' FOR UPDATE",
    )
    .execute(&mut *held)
    .await
    .unwrap();
    let store = db.store.clone();
    let blocked = tokio::spawn(async move {
        record(
            &store,
            "acme",
            "alice",
            "second",
            "second",
            Precondition::Create,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let waiting: i64 = sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name='agent_computer_store_tests' AND wait_event_type='Lock'").fetch_one(&db.pool).await.unwrap();
            if waiting > 0 { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("writer must wait on the organization row lock");
    let independent = tokio::time::timeout(
        Duration::from_secs(5),
        record(
            &db.store,
            "other",
            "alice",
            "first",
            "first",
            Precondition::Create,
        ),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(independent.event_sequence, 1);
    let before = db.store.snapshot(&org("acme")).await.unwrap();
    assert_eq!((before.watermark, before.declarations.len()), (1, 1));
    held.rollback().await.unwrap();
    assert_eq!(blocked.await.unwrap().unwrap().event_sequence, 2);
    let after = db
        .store
        .replay(&org("acme"), before.watermark, 1)
        .await
        .unwrap();
    assert_eq!(after.events[0].sequence, 2);
}

#[tokio::test]
async fn snapshots_have_consistent_watermarks_while_writers_commit() {
    let db = Database::new().await;
    let store = db.store.clone();
    let writer = tokio::spawn(async move {
        for i in 0..30 {
            record(
                &store,
                "acme",
                "alice",
                &format!("key-{i}"),
                &format!("name-{i}"),
                Precondition::Create,
            )
            .await
            .unwrap();
        }
    });
    for _ in 0..40 {
        let snapshot = db.store.snapshot(&org("acme")).await.unwrap();
        assert_eq!(snapshot.watermark as usize, snapshot.declarations.len());
        let after = db
            .store
            .replay(&org("acme"), snapshot.watermark, 100)
            .await
            .unwrap();
        let expected: Vec<i64> = (snapshot.watermark + 1..=after.watermark).collect();
        assert_eq!(
            after.events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
            expected
        );
    }
    writer.await.unwrap();
    assert_eq!(db.store.snapshot(&org("acme")).await.unwrap().watermark, 30);
}
