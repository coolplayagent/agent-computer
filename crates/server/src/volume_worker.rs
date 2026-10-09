use crate::operator::{Failure, failed, private_file, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{Client, Deployment, volume::StorageClassBinding};
use agent_computer_store::{Store, reconciliation::WorkerId};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    api_url: String,
    ca_file: String,
    token_file: String,
    deployment: Deployment,
    storage: StorageClassBinding,
}

pub async fn run(store: &Store, options: &BTreeMap<&str, &str>) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let worker =
        WorkerId::new(*options.get("worker-id").ok_or_else(usage)?).map_err(|_| usage())?;
    let configuration = private_file(options.get("config-file").ok_or_else(usage)?)?;
    let config: Configuration = serde_json::from_str(&configuration)
        .map_err(|_| failed("Invalid private volume worker configuration."))?;
    let ca = private_file(&config.ca_file)?;
    let token = private_file(&config.token_file)?;
    let client = Client::new(
        &config.api_url,
        ca.as_bytes(),
        token.trim(),
        config.deployment,
    )
    .map_err(|_| failed("Invalid Kubernetes transport or deployment configuration."))?;
    let result = agent_computer_worker::reconcile_volume_once(
        store, &client, &config.storage, &org, &worker,
    )
    .await
    .map_err(|_| {
        failed("Volume reconciliation did not commit. Inspect the operation and its current authority before retrying.")
    })?;
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|_| failed("Unable to encode worker result."))?
    );
    Ok(())
}
