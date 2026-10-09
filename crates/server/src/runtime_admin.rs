use crate::operator::{Failure, failed, usage};
use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    Store,
    runtime::{RuntimeGrant, RuntimeKind, RuntimePermission},
};
use std::collections::BTreeMap;

pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let required = |key| options.get(key).copied().ok_or_else(usage);
    let org = OrganizationId::new(required("organization")?).map_err(|_| usage())?;
    let principal = PrincipalId::new(required("principal")?).map_err(|_| usage())?;
    let kind: RuntimeKind = required("kind")?.parse().map_err(|_| usage())?;
    let permission: RuntimePermission = required("permission")?.parse().map_err(|_| usage())?;
    let max_runtime_seconds = options
        .get("max-runtime-seconds")
        .map(|s| s.parse::<u32>().map_err(|_| usage()))
        .transpose()?;
    store
        .set_runtime_grant(
            RuntimeGrant {
                organization: &org,
                principal: &principal,
                kind,
                resource_id: required("resource-id")?,
                permission,
                max_runtime_seconds,
            },
            command == "runtime-grant",
        )
        .await
        .map_err(|_| failed("Runtime grant update failed."))?;
    println!(
        "{}",
        serde_json::json!({"updated":true,"process_termination_confirmed":false})
    );
    Ok(())
}
