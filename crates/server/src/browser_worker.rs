use crate::operator::{Failure, failed, private_file, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_kubernetes::{Client, Deployment};
use agent_computer_store::Store;
use agent_computer_worker::browser::{self, Configuration as WorkerConfiguration};
use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    api_url: String,
    ca_file: String,
    token_file: String,
    deployment: Deployment,
    browser: WorkerConfiguration,
}
pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let id = *options.get("instance-id").ok_or_else(usage)?;
    if command == "browser-drain" {
        store
            .drain_browser_app(&org, id)
            .await
            .map_err(|_| failed("Unable to record Browser drain; reservation remains retained."))?;
        println!(
            "{}",
            serde_json::json!({"instance_id":id,"drain_requested":true,"process_stop_confirmed":false})
        );
        return Ok(());
    }
    if command == "browser-inspect" {
        let entries = store
            .browser_app_journal(&org, id)
            .await
            .map_err(|_| failed("Unable to read Browser journal."))?;
        println!(
            "{}",
            serde_json::to_string(&entries)
                .map_err(|_| failed("Unable to encode Browser journal."))?
        );
        return Ok(());
    }
    let config: Configuration = serde_json::from_str(&private_file(
        options.get("config-file").ok_or_else(usage)?,
    )?)
    .map_err(|_| failed("Invalid private Browser worker configuration."))?;
    let client = Client::new(
        &config.api_url,
        private_file(&config.ca_file)?.as_bytes(),
        private_file(&config.token_file)?.trim(),
        config.deployment,
    )
    .map_err(|_| failed("Invalid Kubernetes Browser transport."))?;
    let revision = options
        .get("expected-revision")
        .ok_or_else(usage)?
        .parse()
        .map_err(|_| usage())?;
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| failed("Unable to install Browser shutdown handler."))?;
    let shutdown = async move {
        tokio::select! { _=tokio::signal::ctrl_c()=>{},_=terminate.recv()=>{} }
    };
    let result=browser::execute_once(store,&client,&org,id,revision,config.browser,shutdown).await.map_err(|_|failed("Browser attempt is unconfirmed. Inspect its original journal; do not repeat Pod creation or launch."))?;
    println!(
        "{}",
        serde_json::to_string(&result).map_err(|_| failed("Unable to encode Browser result."))?
    );
    Ok(())
}
