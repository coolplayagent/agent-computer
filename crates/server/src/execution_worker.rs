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
    let id = if command == "execution-worker" {
        ""
    } else {
        options.get("execution-id").ok_or_else(usage)?
    };
    let config: Configuration = serde_json::from_str(&private_file(
        options.get("config-file").ok_or_else(usage)?,
    )?)
    .map_err(|_| failed("Invalid private execution worker configuration."))?;
    if matches!(
        command,
        "execution-output-recover" | "execution-output-read"
    ) {
        let client = agent_computer_objects::Client::new(&config.execution.outputs)
            .map_err(|_| failed("Invalid private output store configuration."))?;
        if command == "execution-output-read" {
            let bytes = store
                .read_candidate_execution_output(&org, id, &client)
                .await
                .map_err(|_| failed("Durable output was not verified."))?;
            use std::io::Write;
            std::io::stdout()
                .write_all(&bytes)
                .map_err(|_| failed("Unable to write output report."))?;
        } else {
            let spool = agent_computer_objects::Spool::open(&config.execution.output_spool)
                .map_err(|_| failed("Private output spool is unavailable."))?;
            let output=store.recover_candidate_execution_output(&org,id,&client,&spool).await.map_err(|_|failed("Output publication is unconfirmed; retain the original spool and object identities."))?;
            println!(
                "{}",
                serde_json::to_string(&output)
                    .map_err(|_| failed("Unable to encode output metadata."))?
            );
        }
        return Ok(());
    }
    let ca = private_file(&config.ca_file)?;
    let token = private_file(&config.token_file)?;
    let client = Client::new(
        &config.api_url,
        ca.as_bytes(),
        token.trim(),
        config.deployment,
    )
    .map_err(|_| failed("Invalid Kubernetes transport or deployment configuration."))?;
    if command == "execution-worker" {
        let concurrency = options
            .get("concurrency")
            .unwrap_or(&"1")
            .parse()
            .map_err(|_| usage())?;
        let poll_ms = options
            .get("poll-ms")
            .unwrap_or(&"250")
            .parse()
            .map_err(|_| usage())?;
        let options =
            execution::QueueOptions::new(concurrency, std::time::Duration::from_millis(poll_ms))
                .map_err(|_| usage())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| failed("Unable to install worker shutdown handler."))?;
        let shutdown = async move {
            tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
        };
        // Persisted journals remain authoritative if an operator log is lost.
        // Output reports omit user stdout/stderr bytes through WorkResult's serde contract.
        let summary = execution::run_queue(
            store.clone(),
            std::sync::Arc::new(client),
            org,
            config.execution,
            options,
            shutdown,
            |event| {
                if let Ok(line) = serde_json::to_string(&event) {
                    use std::io::Write;
                    let _ = writeln!(std::io::stdout().lock(), "{line}");
                }
            },
        )
        .await
        .map_err(|_| {
            failed(
                "Execution queue worker configuration is not ready; no queue replay is permitted.",
            )
        })?;
        println!(
            "{}",
            serde_json::json!({"event":"stopped","summary":summary})
        );
        return Ok(());
    }
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
