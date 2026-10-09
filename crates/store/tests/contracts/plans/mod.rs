use crate::support::*;
use agent_computer_definitions::{Format, ValidatedDefinition, validate_bytes};
use agent_computer_store::{
    auth::{IssueCredential, PrincipalKind, ServiceScope},
    plans::*,
};
use serde_json::Value;
use std::time::Duration;

mod admission;
mod connections;
mod preparation;
mod publication;
mod reconciliation;
mod references;
mod runtime;
mod starts;
mod writers;

const EXAMPLE: &[u8] = include_bytes!("../../../../../examples/research.computer.yaml");
fn example() -> Value {
    serde_json::to_value(validate_bytes(EXAMPLE, Format::Yaml).unwrap().document()).unwrap()
}
fn checked(value: &Value) -> ValidatedDefinition {
    validate_bytes(&serde_json::to_vec(value).unwrap(), Format::Json).unwrap()
}

async fn fixture() -> (Database, String, Value) {
    let db = Database::new().await;
    let token = credential(&db, "acme", "alice").await;
    for kind in [
        DefinitionKind::Declaration,
        DefinitionKind::Volume,
        DefinitionKind::Workspace,
        DefinitionKind::Sandbox,
        DefinitionKind::App,
        DefinitionKind::Agent,
        DefinitionKind::Computer,
    ] {
        grant(
            &db,
            "acme",
            "alice",
            kind,
            "*",
            DefinitionPermission::Create,
            true,
        )
        .await;
    }
    for (kind, name) in [
        (DefinitionKind::StorageClass, "juicefs-workspace"),
        (DefinitionKind::NetworkPolicy, "public-web-v1"),
        (DefinitionKind::NetworkPolicy, "tool-egress-v1"),
        (DefinitionKind::BrowserProfile, "browser-profile-personal"),
    ] {
        db.store
            .register_catalog_reference(&org("acme"), kind, name)
            .await
            .unwrap();
        grant(
            &db,
            "acme",
            "alice",
            kind,
            name,
            DefinitionPermission::Reference,
            true,
        )
        .await;
    }
    (db, token, example())
}
async fn credential(db: &Database, organization: &str, actor: &str) -> String {
    db.store
        .issue_credential(IssueCredential {
            organization: &org(organization),
            principal: &principal(actor),
            kind: PrincipalKind::Human,
            scopes: &[ServiceScope::DefinitionsManage],
            lifetime: Duration::from_secs(3600),
        })
        .await
        .unwrap()
        .expose_token()
        .to_owned()
}
async fn grant(
    db: &Database,
    organization: &str,
    actor: &str,
    kind: DefinitionKind,
    name: &str,
    permission: DefinitionPermission,
    enabled: bool,
) {
    db.store
        .set_definition_grant(
            DefinitionGrant {
                organization: &org(organization),
                principal: &principal(actor),
                kind,
                name,
                permission,
            },
            enabled,
        )
        .await
        .unwrap();
}
fn revisions(value: &mut Value, plan: &DefinitionPlan, root: u64) {
    value["metadata"]["expectedRevision"] = root.into();
    for (collection, kind) in [
        ("volumes", DefinitionKind::Volume),
        ("workspaces", DefinitionKind::Workspace),
        ("sandboxes", DefinitionKind::Sandbox),
        ("apps", DefinitionKind::App),
        ("agents", DefinitionKind::Agent),
        ("computers", DefinitionKind::Computer),
    ] {
        for object in value["spec"][collection].as_array_mut().unwrap() {
            let resource = plan
                .resources
                .iter()
                .find(|r| r.kind == kind && r.name == object["name"])
                .unwrap();
            object["expectedRevision"] = resource.revision.into();
        }
    }
}
async fn count(db: &Database, table: &str) -> i64 {
    sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&db.pool)
        .await
        .unwrap()
}
