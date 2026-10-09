use crate::operator::{Failure, failed, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_store::Store;
use std::collections::BTreeMap;

pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let operation = options.get("operation").ok_or_else(usage)?;
    match command {
        "reconciliation-resume"=>store.resume_reconciliation(&org,operation).await.map_err(|_|failed("Resume failed. Check admission authority and blocked state."))?,
        "reconciliation-abandon"=>store.abandon_reconciliation(&org,operation).await.map_err(|_|failed("Abandon failed. Only blocked work without unresolved effects can be abandoned."))?,
        _=>{},
    }
    let status = store
        .inspect_reconciliation(&org, operation)
        .await
        .map_err(|_| failed("Operation unavailable in this organization."))?;
    println!(
        "{}",
        serde_json::to_string(&status)
            .map_err(|_| failed("Unable to serialize reconciliation status."))?
    );
    Ok(())
}
