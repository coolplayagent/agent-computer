use crate::operator::{Failure, failed, private_file, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{Client, Deployment};
use agent_computer_store::Store;
use agent_computer_worker::execution::{self, Configuration as WorkerConfiguration};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    api_url: String,
    ca_file: String,
    token_file: String,
    deployment: Deployment,
    execution: WorkerConfiguration,
}
pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let id = options.get("execution-id").ok_or_else(usage)?;
    let config: Configuration = serde_json::from_str(&private_file(
        options.get("config-file").ok_or_else(usage)?,
    )?)
    .map_err(|_| failed("Invalid private execution worker configuration."))?;
    let ca = private_file(&config.ca_file)?;
    let token = private_file(&config.token_file)?;
    let client = Client::new(
        &config.api_url,
        ca.as_bytes(),
        token.trim(),
        config.deployment,
    )
    .map_err(|_| failed("Invalid Kubernetes transport or deployment configuration."))?;
    let result=if command=="execution-dispatch-once" {
        let revision=options.get("expected-revision").ok_or_else(usage)?.parse().map_err(|_|usage())?;
        execution::execute_once(store,&client,&org,id,revision,config.execution).await
    } else {
        execution::recover_once(store,&client,&org,id,&config.execution.storage,&config.execution.approved_supervisor_image,&config.execution.node.spool).await
    }.map_err(|_|failed("Execution worker did not acknowledge a result. Inspect the original execution and Pod journal; recovery never reissues creation or startup authorization."))?;
    println!(
        "{}",
        serde_json::to_string(&result)
            .map_err(|_| failed("Unable to encode execution worker observation."))?
    );
    Ok(())
}
