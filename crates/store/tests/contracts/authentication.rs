use crate::support::*;
use agent_computer_store::{
    Error,
    auth::{IssueCredential, PrincipalKind, ServiceScope},
};
use std::time::Duration;

#[tokio::test]
async fn credentials_bind_identity_and_scopes_and_store_only_hashes() {
    let db = Database::new().await;
    let organization = org("acme");
    let actor = principal("worker");
    let request = || IssueCredential {
        organization: &organization,
        principal: &actor,
        kind: PrincipalKind::Agent,
        scopes: &[ServiceScope::DefinitionsValidate],
        lifetime: Duration::from_secs(3600),
    };
    let first = db
        .store
        .issue_credential(IssueCredential {
            organization: &organization,
            principal: &actor,
            ..request()
        })
        .await
        .unwrap();
    let second = db
        .store
        .issue_credential(IssueCredential {
            organization: &organization,
            principal: &actor,
            ..request()
        })
        .await
        .unwrap();
    assert_ne!(first.expose_token(), second.expose_token());
    assert!(!format!("{first:?}").contains(first.expose_token()));
    let row: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM service_credentials c WHERE credential_id=$1")
            .bind(first.id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(!row.to_string().contains(first.expose_token()));
    assert!(row.get("token").is_none());
    let auth = db
        .store
        .authorize_service(first.expose_token(), ServiceScope::DefinitionsValidate)
        .await
        .unwrap();
    assert_eq!(auth.organization(), &organization);
    assert_eq!(auth.principal(), &actor);
    assert_eq!(auth.kind(), PrincipalKind::Agent);
    assert!(matches!(
        db.store
            .authorize_service(first.expose_token(), ServiceScope::DefinitionsManage)
            .await,
        Err(Error::Forbidden)
    ));
    let mut wrong = first.expose_token().to_owned();
    let tail = if wrong.ends_with('0') { "1" } else { "0" };
    wrong.replace_range(wrong.len() - 1.., tail);
    for token in [wrong.as_str(), "", "garbage", "acsk_x_y"] {
        assert!(matches!(
            db.store
                .authorize_service(token, ServiceScope::DefinitionsValidate)
                .await,
            Err(Error::Unauthenticated)
        ));
    }
}

#[tokio::test]
async fn revocation_expiry_and_principal_disable_are_checked_without_cache() {
    let db = Database::new().await;
    let organization = org("acme");
    let actor = principal("human");
    let issue = || IssueCredential {
        organization: &organization,
        principal: &actor,
        kind: PrincipalKind::Human,
        scopes: &[ServiceScope::DefinitionsValidate],
        lifetime: Duration::from_secs(3600),
    };
    let revoked = db.store.issue_credential(issue()).await.unwrap();
    let expired = db.store.issue_credential(issue()).await.unwrap();
    let disabled = db.store.issue_credential(issue()).await.unwrap();
    assert!(
        !db.store
            .revoke_credential(&org("other"), revoked.id())
            .await
            .unwrap()
    );
    db.store
        .authorize_service(revoked.expose_token(), ServiceScope::DefinitionsValidate)
        .await
        .unwrap();
    assert!(
        db.store
            .revoke_credential(&organization, revoked.id())
            .await
            .unwrap()
    );
    assert!(matches!(
        db.store
            .authorize_service(revoked.expose_token(), ServiceScope::DefinitionsValidate)
            .await,
        Err(Error::Unauthenticated)
    ));
    sqlx::query("UPDATE service_credentials SET issued_at=clock_timestamp()-interval '2 seconds',expires_at=clock_timestamp()-interval '1 second' WHERE credential_id=$1").bind(expired.id()).execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .authorize_service(expired.expose_token(), ServiceScope::DefinitionsValidate)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert!(
        db.store
            .disable_principal(&organization, &actor)
            .await
            .unwrap()
    );
    assert!(matches!(
        db.store
            .authorize_service(disabled.expose_token(), ServiceScope::DefinitionsValidate)
            .await,
        Err(Error::Unauthenticated)
    ));
    assert!(matches!(
        db.store.issue_credential(issue()).await,
        Err(Error::PrincipalConflict)
    ));
}

#[tokio::test]
async fn credential_bounds_kind_conflicts_and_schema_readiness() {
    let db = Database::new().await;
    db.store.ready().await.unwrap();
    let organization = org("acme");
    let actor = principal("person");
    let issue = || IssueCredential {
        organization: &organization,
        principal: &actor,
        kind: PrincipalKind::Human,
        scopes: &[ServiceScope::DefinitionsValidate],
        lifetime: Duration::from_secs(1),
    };
    for lifetime in [
        Duration::ZERO,
        Duration::from_secs(86401),
        Duration::from_millis(1500),
    ] {
        assert!(matches!(
            db.store
                .issue_credential(IssueCredential {
                    lifetime,
                    ..issue()
                })
                .await,
            Err(Error::InvalidCredentialParameters)
        ));
    }
    assert!(matches!(
        db.store
            .issue_credential(IssueCredential {
                scopes: &[],
                ..issue()
            })
            .await,
        Err(Error::InvalidCredentialParameters)
    ));
    db.store.issue_credential(issue()).await.unwrap();
    assert!(matches!(
        db.store
            .issue_credential(IssueCredential {
                kind: PrincipalKind::Agent,
                ..issue()
            })
            .await,
        Err(Error::PrincipalConflict)
    ));
    sqlx::query("UPDATE _sqlx_migrations SET checksum=decode('00','hex') WHERE version=2")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
}

#[tokio::test]
async fn runtime_scope_migration_preserves_existing_credentials_without_granting_access() {
    let db = Database::new().await;
    // Reconstruct the exact predecessor constraint/history in this disposable
    // database, with no runtime rows. No production downgrade API is provided.
    sqlx::raw_sql("DROP TABLE runtime_grants; ALTER TABLE service_credentials DROP CONSTRAINT service_credentials_scopes_check; ALTER TABLE service_credentials ADD CONSTRAINT service_credentials_scopes_check CHECK (cardinality(scopes) BETWEEN 1 AND 2 AND scopes <@ ARRAY['definitions.validate','definitions.manage']::TEXT[]); DELETE FROM _sqlx_migrations WHERE version=6;")
        .execute(&db.pool).await.unwrap();
    let credential = db
        .store
        .issue_credential(IssueCredential {
            organization: &org("acme"),
            principal: &principal("legacy"),
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::DefinitionsManage],
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap();
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM service_credentials c WHERE credential_id=$1")
            .bind(credential.id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert!(matches!(db.store.ready().await, Err(Error::SchemaNotReady)));
    db.store.migrate().await.unwrap();
    db.store.ready().await.unwrap();
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT to_jsonb(c) FROM service_credentials c WHERE credential_id=$1")
            .bind(credential.id())
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    db.store
        .authorize_service(credential.expose_token(), ServiceScope::DefinitionsManage)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .authorize_service(credential.expose_token(), ServiceScope::RuntimeRead)
            .await,
        Err(Error::Forbidden)
    ));
    let grants: i64 = sqlx::query_scalar("SELECT count(*) FROM runtime_grants")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(grants, 0);
}
