use super::*;
use agent_computer_store::Error;

#[tokio::test]
async fn reference_grant_removal_and_catalog_disable_invalidate_pending_plan() {
    let (db, token, document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("plan"), &checked(&document))
        .await
        .unwrap();
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::BrowserProfile,
        "browser-profile-personal",
        DefinitionPermission::Reference,
        false,
    )
    .await;
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::BrowserProfile,
        "browser-profile-personal",
        DefinitionPermission::Reference,
        true,
    )
    .await;
    let reference = plan
        .resources
        .iter()
        .flat_map(|r| &r.dependencies)
        .find(|d| d.kind == DefinitionKind::NetworkPolicy)
        .unwrap();
    db.store
        .disable_catalog_reference(&org("acme"), &reference.resource_id)
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    assert_eq!(count(&db, "operations").await, 0);
}

#[tokio::test]
async fn explicit_ids_are_kind_and_organization_checked_and_preserve_digest_equivalence() {
    let (db, token, mut document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("first"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
        .await
        .unwrap();
    revisions(&mut document, &plan, 1);
    let workspace = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Workspace)
        .unwrap();
    document["spec"]["computers"][0]["workspaceRef"] =
        format!("id:{}", workspace.resource_id).into();
    let equivalent = db
        .store
        .create_definition_plan(&token, &key("id-form"), &checked(&document))
        .await
        .unwrap();
    assert!(
        equivalent
            .resources
            .iter()
            .all(|r| r.change == Change::Unchanged)
    );
    let app = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::App)
        .unwrap();
    document["spec"]["computers"][0]["workspaceRef"] = format!("id:{}", app.resource_id).into();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("wrong-kind"), &checked(&document))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    let foreign = db
        .store
        .register_catalog_reference(
            &org("foreign"),
            DefinitionKind::StorageClass,
            "private-storage",
        )
        .await
        .unwrap();
    // Use new resource names to ensure the rejection comes from reference scope.
    let mut only = serde_json::json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"foreign-ref"},"spec":{"volumes":[{"name":"fresh","storageClass":format!("id:{foreign}"),"quotaBytes":1,"reclaimPolicy":"Retain"}]}});
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("foreign"), &checked(&only))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    only["spec"]["volumes"][0]["storageClass"] = "missing".into();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("missing"), &checked(&only))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
}

#[tokio::test]
async fn external_app_must_use_a_sandbox_attached_to_the_computer() {
    let (db, token, mut document) = fixture().await;
    let plan = db
        .store
        .create_definition_plan(&token, &key("first"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(&token, &key("apply"), &plan.plan_id, &plan.plan_digest)
        .await
        .unwrap();
    let app = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::App)
        .unwrap();
    let workspace = plan
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Workspace)
        .unwrap();
    document["metadata"] = serde_json::json!({"name":"other-computer"});
    document["spec"] = serde_json::json!({"computers":[{"name":"isolated","workspaceRef":format!("id:{}",workspace.resource_id),"sandboxRefs":[],"appRefs":[format!("id:{}",app.resource_id)],"desiredState":"Stopped"}]});
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("incompatible"), &checked(&document))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
}

#[tokio::test]
async fn existing_app_cannot_bypass_profile_grants_or_pin_a_different_sandbox_version() {
    let (db, token, document) = fixture().await;
    let original = db
        .store
        .create_definition_plan(&token, &key("first"), &checked(&document))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &token,
            &key("apply"),
            &original.plan_id,
            &original.plan_digest,
        )
        .await
        .unwrap();
    let app = original
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::App)
        .unwrap();
    let sandbox = original
        .resources
        .iter()
        .find(|r| r.name == "browser-env")
        .unwrap();
    let workspace = original
        .resources
        .iter()
        .find(|r| r.kind == DefinitionKind::Workspace)
        .unwrap();
    let composed = serde_json::json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"composed"},"spec":{"computers":[{"name":"composed","workspaceRef":format!("id:{}",workspace.resource_id),"sandboxRefs":[format!("id:{}",sandbox.resource_id)],"appRefs":[format!("id:{}",app.resource_id)],"desiredState":"Stopped"}]}});
    db.store
        .create_definition_plan(&token, &key("composed"), &checked(&composed))
        .await
        .unwrap();
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::BrowserProfile,
        "browser-profile-personal",
        DefinitionPermission::Reference,
        false,
    )
    .await;
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("private"), &checked(&composed))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
    grant(
        &db,
        "acme",
        "alice",
        DefinitionKind::BrowserProfile,
        "browser-profile-personal",
        DefinitionPermission::Reference,
        true,
    )
    .await;
    let mut updated = document["spec"]["sandboxes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "browser-env")
        .unwrap()
        .clone();
    updated["expectedRevision"] = 1.into();
    updated["image"] = format!("registry.example.invalid/browser@sha256:{}", "c".repeat(64)).into();
    let update = serde_json::json!({"apiVersion":"agent-computer/v1alpha1","kind":"ComputerSet","metadata":{"name":"sandbox-upgrade"},"spec":{"sandboxes":[updated]}});
    let plan = db
        .store
        .create_definition_plan(&token, &key("upgrade"), &checked(&update))
        .await
        .unwrap();
    db.store
        .apply_definition_plan(
            &token,
            &key("upgrade-apply"),
            &plan.plan_id,
            &plan.plan_digest,
        )
        .await
        .unwrap();
    assert!(matches!(
        db.store
            .create_definition_plan(&token, &key("mismatch"), &checked(&composed))
            .await,
        Err(Error::ReferenceUnavailable)
    ));
}
