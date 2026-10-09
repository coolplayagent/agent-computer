use crate::support::*;
use agent_computer_store::{Error, Precondition};

#[tokio::test]
async fn bounded_replay_retention_and_at_least_once_outbox() {
    let db = Database::new().await;
    for name in ["one", "two", "three"] {
        record(&db.store, "acme", "alice", name, name, Precondition::Create)
            .await
            .unwrap();
    }
    let snapshot = db.store.snapshot(&org("acme")).await.unwrap();
    assert_eq!(snapshot.watermark, 3);
    assert_eq!(snapshot.declarations.len(), 3);
    let page = db.store.replay(&org("acme"), 0, 1).await.unwrap();
    assert_eq!((page.next_cursor, page.watermark), (1, 3));
    let next = db
        .store
        .replay(&org("acme"), page.next_cursor, 1)
        .await
        .unwrap();
    assert_eq!(next.events[0].sequence, 2);
    assert_eq!(next.events[0].kind, "declaration.recorded");
    assert!(next.events[0].payload.get("canonical").is_none());
    let pending = db.store.pending_outbox(&org("acme"), 2).await.unwrap();
    assert_eq!(
        pending,
        db.store.pending_outbox(&org("acme"), 2).await.unwrap()
    );
    assert!(matches!(
        db.store.prune_events(&org("acme"), 2).await,
        Err(Error::UnpublishedEvents)
    ));
    assert!(!db.store.acknowledge(&org("other"), 1).await.unwrap());
    assert!(db.store.acknowledge(&org("acme"), 1).await.unwrap());
    assert!(db.store.acknowledge(&org("acme"), 1).await.unwrap());
    assert!(matches!(
        db.store.prune_events(&org("acme"), 2).await,
        Err(Error::UnpublishedEvents)
    ));
    db.store.acknowledge(&org("acme"), 2).await.unwrap();
    db.store.prune_events(&org("acme"), 2).await.unwrap();
    assert!(matches!(
        db.store.replay(&org("acme"), 1, 100).await,
        Err(Error::CursorExpired)
    ));
    assert_eq!(
        db.store.replay(&org("acme"), 2, 100).await.unwrap().events[0].sequence,
        3
    );
    assert_eq!(
        db.store
            .snapshot(&org("acme"))
            .await
            .unwrap()
            .declarations
            .len(),
        3
    );
    let retry = record(
        &db.store,
        "acme",
        "alice",
        "one",
        "one",
        Precondition::Create,
    )
    .await
    .unwrap();
    assert_eq!(retry.event_sequence, 1); // Event retention never recycles request keys.
    for cursor in [-1, 4] {
        assert!(matches!(
            db.store.replay(&org("acme"), cursor, 1).await,
            Err(Error::InvalidCursor)
        ));
    }
    for limit in [0, 1001] {
        assert!(matches!(
            db.store.replay(&org("acme"), 2, limit).await,
            Err(Error::InvalidPageSize)
        ));
    }
    assert!(matches!(
        db.store.prune_events(&org("acme"), 1).await,
        Err(Error::InvalidCursor)
    ));
}
