use crate::operator::{Failure, failed, private_file, usage};
use agent_computer_core::identity::OrganizationId;
use agent_computer_store::{Store, reconciliation::WorkerId};
use std::collections::BTreeMap;

pub async fn run(
    command: &str,
    store: &Store,
    options: &BTreeMap<&str, &str>,
) -> Result<(), Failure> {
    let org = OrganizationId::new(*options.get("organization").ok_or_else(usage)?)
        .map_err(|_| usage())?;
    let owner = WorkerId::new(*options.get("worker-id").ok_or_else(usage)?).map_err(|_| usage())?;
    let bytes = private_file(options.get("config-file").ok_or_else(usage)?)?;
    let config = serde_json::from_str(&bytes)
        .map_err(|_| failed("Invalid private Candidate worker configuration."))?;
    if command == "candidate-worker" {
        use agent_computer_worker::candidate::{QueueOptions, run_queue};
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
        let options = QueueOptions::new(concurrency, std::time::Duration::from_millis(poll_ms))
            .map_err(|_| usage())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|_| failed("Unable to install Candidate worker shutdown handler."))?;
        let shutdown = async move {
            tokio::select! { _=tokio::signal::ctrl_c()=>{}, _=terminate.recv()=>{} }
        };
        let summary = run_queue(
            store.clone(),
            org,
            owner,
            config,
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
            failed("Candidate worker configuration is unavailable; no preparation was scheduled.")
        })?;
        println!(
            "{}",
            serde_json::json!({"event":"stopped","summary":summary})
        );
        return Ok(());
    }
    let request = options.get("request-id").ok_or_else(usage)?;
    let outcome = agent_computer_worker::candidate::prepare_once(store, &org, request, &owner, config).await.map_err(|_|failed("Candidate preparation did not commit. Keep its identity and reservations; inspect current authority and storage before retrying."))?;
    println!(
        "{}",
        serde_json::to_string(&outcome)
            .map_err(|_| failed("Unable to encode preparation outcome."))?
    );
    Ok(())
}
