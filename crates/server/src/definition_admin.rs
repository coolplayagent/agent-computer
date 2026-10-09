use crate::operator::{Failure, failed, usage};
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    Store,
    plans::{DefinitionGrant, DefinitionKind, DefinitionPermission},
};
use std::collections::BTreeMap;

pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let required = |key| options.get(key).copied().ok_or_else(usage);
    let org = OrganizationId::new(required("organization")?).map_err(|_| usage())?;
    if command == "catalog-disable" {
        if !store
            .disable_catalog_reference(&org, required("resource-id")?)
            .await
            .map_err(|_| failed("Catalog update failed."))?
        {
            return Err(failed(
                "Catalog reference was not enabled in this organization.",
            ));
        }
        println!("{}", serde_json::json!({"disabled":true}));
        return Ok(());
    }
    let kind: DefinitionKind = required("kind")?.parse().map_err(|_| usage())?;
    let name = required("name")?;
    if command == "catalog-register" {
        let id = store
            .register_catalog_reference(&org, kind, name)
            .await
            .map_err(|_| failed("Catalog registration failed."))?;
        println!(
            "{}",
            serde_json::json!({"resource_id":id,"kind":kind,"name":name,"scope":"reference-metadata"})
        );
    } else {
        let principal = PrincipalId::new(required("principal")?).map_err(|_| usage())?;
        let permission = match required("permission")? {
            "create" => DefinitionPermission::Create,
            "manage" => DefinitionPermission::Manage,
            "reference" => DefinitionPermission::Reference,
            _ => return Err(usage()),
        };
        store
            .set_definition_grant(
                DefinitionGrant {
                    organization: &org,
                    principal: &principal,
                    kind,
                    name,
                    permission,
                },
                command == "definition-grant",
            )
            .await
            .map_err(|_| failed("Definition grant update failed."))?;
        println!("{}", serde_json::json!({"updated":true}));
    }
    Ok(())
}
