use super::*;
use agent_computer_store::{Error, auth::ServiceScope};

#[tokio::test]
async fn service_scope_never_substitutes_for_grants_and_plans_are_principal_scoped() {
    let (db, token, document) = fixture().await;
    let stranger = credential(&db, "acme", "stranger").await;
    assert!(matches!(
        db.store
            .create_definition_plan(&stranger, &key("forbidden"), &checked(&document))
            .await,
        Err(Error::Forbidden)
    ));
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    assert!(matches!(
        db.store.definition_plan(&stranger, &plan.plan_id).await,
        Err(Error::PlanNotFound)
    ));
    let foreign = credential(&db, "foreign", "alice").await;
    assert!(matches!(
        db.store
            .apply_definition_plan(&foreign, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::PlanNotFound)
    ));
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::Sandbox,
        "*",
        DefinitionPermission::Create,
        false,
    )
    .await;
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::Forbidden)
    ));
    assert_eq!(count(&db, "resource_definitions").await, 0);
}

#[tokio::test]
async fn revocation_winning_the_admission_lock_prevents_all_apply_writes() {
    let (db, token, document) = fixture().await;
    let identity = db
        .store
        .authorize_service(&token, ServiceScope::DefinitionsManage)
        .await
        .unwrap();
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    let mut revoke = db.pool.begin().await.unwrap();
    sqlx::query(
        "UPDATE service_credentials SET revoked=TRUE WHERE organization=$1 AND principal=$2",
    )
    .bind(identity.organization().as_str())
    .bind(identity.principal().as_str())
    .execute(&mut *revoke)
    .await
    .unwrap();
    let store = db.store.clone();
    let applying = tokio::spawn(async move {
        store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await
    });
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE application_name='agent_computer_store_tests' AND wait_event_type='Lock'").fetch_one(&db.pool).await.unwrap();
            if waiting>0 {break;}
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    revoke.commit().await.unwrap();
    assert!(matches!(
        applying.await.unwrap(),
        Err(Error::Unauthenticated)
    ));
    assert_eq!(count(&db, "operations").await, 0);
    assert_eq!(count(&db, "resource_spec_versions").await, 0);
}

#[tokio::test]
async fn retired_plan_and_apply_request_keys_never_start_new_work() {
    let (db, token, document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    let operation = db
        .store
        .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
        .await
        .unwrap();
    sqlx::query("UPDATE request_records SET retired=TRUE,response=NULL WHERE operation IN ('definitions.plan.v1','definitions.apply.v1')").execute(&db.pool).await.unwrap();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("plan"), &checked(&document))
            .await,
        Err(Error::IdempotencyGone)
    ));
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::IdempotencyGone)
    ));
    assert_eq!(count(&db, "operations").await, 1);
    assert_eq!(
        db.store
            .definition_operation(&token, &operation.operation_id)
            .await
            .unwrap()
            .state,
        "Queued"
    );
}

#[tokio::test]
async fn expired_plan_is_rejected_against_database_time() {
    let (db, token, document) = fixture().await;
    let mut plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    // Isolated database fixture: age a valid immutable plan without a 15-minute
    // wall-clock test. This requires database-owner privileges, never the API.
    plan.expires_at_ms = 1;
    plan.plan_digest.clear();
    use sha2::{Digest, Sha256};
    let mut hash = Sha256::new();
    hash.update(b"agent-computer/definition-plan-v1\0");
    hash.update(serde_json::to_vec(&serde_json::to_value(&plan).unwrap()).unwrap());
    plan.plan_digest = format!("sha256:{:x}", hash.finalize());
    sqlx::query("ALTER TABLE definition_plans DISABLE TRIGGER immutable_definition_plan")
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE definition_plans SET preview=$1,digest=$2,expires_at_ms=1")
        .bind(serde_json::to_value(&plan).unwrap())
        .bind(&plan.plan_digest)
        .execute(&db.pool)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE definition_plans ENABLE TRIGGER immutable_definition_plan")
        .execute(&db.pool)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::PlanExpired)
    ));
    assert_eq!(count(&db, "resource_definitions").await, 0);
}
