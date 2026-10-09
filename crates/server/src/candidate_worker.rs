use crate::operator::{Failure, failed, private_file, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_store::{Store, reconciliation::WorkerId};
use std::collections::BTreeMap;

pub async fn run(store: &Store, options: &BTreeMap<&str, &str>) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let owner = WorkerId::new(*options.get("worker-id").ok_or_else(usage)?).map_err(|_| usage())?;
    let request = options.get("request-id").ok_or_else(usage)?;
    let bytes = private_file(options.get("config-file").ok_or_else(usage)?)?;
    let config = serde_json::from_str(&bytes)
        .map_err(|_| failed("Invalid private Candidate worker configuration."))?;
    let outcome = agent_computer_worker::candidate::prepare_once(store, &org, request, &owner, config).await.map_err(|_|failed("Candidate preparation did not commit. Keep its identity and reservations; inspect current authority and storage before retrying."))?;
    println!(
        "{}",
        serde_json::to_string(&outcome)
            .map_err(|_| failed("Unable to encode preparation outcome."))?
    );
    Ok(())
}
