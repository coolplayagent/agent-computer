use agent_computer_core::identity::{OrganizationId, PrincipalId};
use agent_computer_store::{
    Store,
    auth::{IssueCredential, PrincipalKind, ServiceScope},
};
use sqlx::postgres::PgPoolOptions;
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::PathBuf,
    time::Duration,
};

pub struct Failure {
    pub exit: u8,
    pub message: &'static str,
}
pub(crate) fn usage() -> Failure {
    Failure {
        exit: 2,
        message: "Invalid arguments. Run agent-computer-server --help.",
    }
}
pub(crate) fn failed(message: &'static str) -> Failure {
    Failure { exit: 1, message }
}

pub async fn run(args: Vec<String>) -> Result<(), Failure> {
    if args.is_empty() || args == ["--help"] || args == ["help"] {
        println!(
            "agent-computer-server\n\nCommands:\n  migrate --database-url-file PATH\n  serve --database-url-file PATH [--listen 127.0.0.1:8080] [--file-config PATH]\n  credential-issue --database-url-file PATH --organization ID --principal ID --kind human|agent --scopes SCOPE[,SCOPE...] --ttl-seconds 3600 --output PATH\n  credential-revoke --database-url-file PATH --organization ID --credential ID\n  principal-disable --database-url-file PATH --organization ID --principal ID\n  definition-grant|definition-revoke --database-url-file PATH --organization ID --principal ID --kind KIND --name NAME --permission create|manage|reference\n  runtime-grant|runtime-revoke --database-url-file PATH --organization ID --principal ID --kind computer|workspace|app|browser_profile --resource-id ID --permission PERMISSION [--max-runtime-seconds SECONDS]\n  catalog-register --database-url-file PATH --organization ID --kind KIND --name NAME\n  catalog-disable --database-url-file PATH --organization ID --resource-id ID\n  reconciliation-inspect|reconciliation-resume|reconciliation-abandon --database-url-file PATH --organization ID --operation ID\n  reconciliation-volumes-once --database-url-file PATH --organization ID --worker-id ID --config-file PATH\n  artifact-publish-once --database-url-file PATH --organization ID --worker-id ID --commit-id ID --config-file PATH\n  candidate-prepare-once --database-url-file PATH --organization ID --worker-id ID --request-id ID --config-file PATH\n  execution-dispatch-once --database-url-file PATH --organization ID --execution-id ID --expected-revision REVISION --config-file PATH\n  execution-recover-once|execution-output-recover|execution-output-read --database-url-file PATH --organization ID --execution-id ID --config-file PATH\n  workspace-initialize-empty --database-url-file PATH --organization ID --workspace-id ID\n  writer-lease-reconcile --database-url-file PATH --organization ID --lease-id ID\n  candidate-file-save-once --database-url-file PATH --credential-file PATH --lease-id ID --request-file PATH --config-file PATH\n\nScopes: definitions.validate, definitions.manage, runtime.connect, runtime.read, runtime.observe, runtime.app.use, runtime.activate, runtime.execute, runtime.modify, runtime.control, runtime.publish, runtime.manage, runtime.delete.\nRuntime activate grants require a 1..86400 second limit; revoke omits the limit.\n\nCredential administration requires trusted database access. Secret files must be private. Remote access requires a TLS reverse proxy; OIDC and Computer runtime are not implemented."
        );
        return Ok(());
    }
    let command = args[0].as_str();
    let allowed: &[&str] = match command {
        "migrate" => &["database-url-file"],
        "serve" => &["database-url-file", "listen", "file-config"],
        "credential-issue" => &[
            "database-url-file",
            "organization",
            "principal",
            "kind",
            "scopes",
            "ttl-seconds",
            "output",
        ],
        "credential-revoke" => &["database-url-file", "organization", "credential"],
        "principal-disable" => &["database-url-file", "organization", "principal"],
        "definition-grant" | "definition-revoke" => &[
            "database-url-file",
            "organization",
            "principal",
            "kind",
            "name",
            "permission",
        ],
        "runtime-grant" | "runtime-revoke" => &[
            "database-url-file",
            "organization",
            "principal",
            "kind",
            "resource-id",
            "permission",
            "max-runtime-seconds",
        ],
        "catalog-register" => &["database-url-file", "organization", "kind", "name"],
        "catalog-disable" => &["database-url-file", "organization", "resource-id"],
        "reconciliation-inspect" | "reconciliation-resume" | "reconciliation-abandon" => {
            &["database-url-file", "organization", "operation"]
        }
        "reconciliation-volumes-once" => &[
            "database-url-file",
            "organization",
            "worker-id",
            "config-file",
        ],
        "artifact-publish-once" => &[
            "database-url-file",
            "organization",
            "worker-id",
            "commit-id",
            "config-file",
        ],
        "candidate-prepare-once" => &[
            "database-url-file",
            "organization",
            "worker-id",
            "request-id",
            "config-file",
        ],
        "execution-dispatch-once" => &[
            "database-url-file",
            "organization",
            "execution-id",
            "expected-revision",
            "config-file",
        ],
        "execution-recover-once" | "execution-output-recover" | "execution-output-read" => &[
            "database-url-file",
            "organization",
            "execution-id",
            "config-file",
        ],
        "workspace-initialize-empty" => &["database-url-file", "organization", "workspace-id"],
        "writer-lease-reconcile" => &["database-url-file", "organization", "lease-id"],
        "candidate-file-save-once" => &[
            "database-url-file",
            "credential-file",
            "lease-id",
            "request-file",
            "config-file",
        ],
        _ => return Err(usage()),
    };
    if !(args.len() - 1).is_multiple_of(2) {
        return Err(usage());
    }
    let mut options = BTreeMap::new();
    for pair in args[1..].chunks_exact(2) {
        let name = pair[0].strip_prefix("--").ok_or_else(usage)?;
        if !allowed.contains(&name) || options.insert(name, pair[1].as_str()).is_some() {
            return Err(usage());
        }
    }
    let required = |key| options.get(key).copied().ok_or_else(usage);
    let url = private_file(required("database-url-file")?)?;
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url.trim())
        .await
        .map_err(|_| failed("Unable to connect to the database."))?;
    let store = Store::new(pool.clone());
    if command == "migrate" {
        store
            .migrate()
            .await
            .map_err(|_| failed("Database migration failed."))?;
        println!("Database migrations complete.");
    } else {
        store
            .ready()
            .await
            .map_err(|_| failed("Database schema is not ready. Run migrate first."))?;
        match command {
            "execution-dispatch-once"
            | "execution-recover-once"
            | "execution-output-recover"
            | "execution-output-read" => {
                crate::execution_worker::run(command, &store, &options).await?
            }
            "candidate-prepare-once" => crate::candidate_worker::run(&store, &options).await?,
            "artifact-publish-once" => {
                let org = OrganizationId::new(required("organization")?).map_err(|_| usage())?;
                let owner =
                    agent_computer_store::reconciliation::WorkerId::new(required("worker-id")?)
                        .map_err(|_| usage())?;
                let config = serde_json::from_str(&private_file(required("config-file")?)?)
                    .map_err(|_| failed("Invalid artifact worker configuration."))?;
                let result=agent_computer_worker::artifacts::publish_once(&store,&org,required("commit-id")?,&owner,config).await.map_err(|_|failed("Artifact publication is unconfirmed. Query the original commit; retain its Candidate and retry the same identity."))?;
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|_| failed("Unable to encode artifact outcome."))?
                );
            }
            "candidate-file-save-once" => {
                let token = private_file(required("credential-file")?)?;
                let config = serde_json::from_str(&private_file(required("config-file")?)?)
                    .map_err(|_| failed("Invalid private file worker configuration."))?;
                let input = agent_computer_worker::files::read_request(&working_path(required(
                    "request-file",
                )?))
                .map_err(|_| failed("Invalid private file save request."))?;
                let result = agent_computer_worker::files::save_once(&store,token.trim(),required("lease-id")?,input,config).await.map_err(|_|failed("File save was not acknowledged. Query the existing lease/dispatch; do not issue a replacement write for an unknown outcome."))?;
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|_| failed("Unable to encode file save result."))?
                );
            }
            "writer-lease-reconcile" => {
                let org = OrganizationId::new(required("organization")?).map_err(|_| usage())?;
                let result=store.reconcile_candidate_writer(&org,required("lease-id")?).await.map_err(|_|failed("Writer lease reconciliation failed; no stopped-process assertion is accepted."))?;
                println!(
                    "{}",
                    serde_json::to_string(&result)
                        .map_err(|_| failed("Unable to encode writer lease state."))?
                );
            }
            "workspace-initialize-empty" => {
                let org = OrganizationId::new(required("organization")?).map_err(|_| usage())?;
                let revision = store
                    .initialize_empty_workspace(&org, required("workspace-id")?)
                    .await
                    .map_err(|_| {
                        failed(
                            "Workspace input initialization failed; existing input is never reset.",
                        )
                    })?;
                println!("{{\"input_revision\":{revision}}}");
            }
            "runtime-grant" | "runtime-revoke" => {
                crate::runtime_admin::run(command, &store, &options).await?
            }
            "reconciliation-volumes-once" => crate::volume_worker::run(&store, &options).await?,
            "reconciliation-inspect" | "reconciliation-resume" | "reconciliation-abandon" => {
                crate::reconciliation_admin::run(command, &store, &options).await?
            }
            "definition-grant" | "definition-revoke" | "catalog-register" | "catalog-disable" => {
                crate::definition_admin::run(command, &store, &options).await?
            }
            "serve" => {
                let router = if let Some(path) = options.get("file-config") {
                    let config = serde_json::from_str(&private_file(path)?)
                        .map_err(|_| failed("Invalid private file gateway configuration."))?;
                    let gateway = agent_computer_worker::files::FileGateway::new(config)
                        .map_err(|_| failed("Invalid private file gateway configuration."))?;
                    agent_computer_server::router_with_files(store, gateway)
                } else {
                    agent_computer_server::router(store)
                };
                let address: SocketAddr = options
                    .get("listen")
                    .unwrap_or(&"127.0.0.1:8080")
                    .parse()
                    .map_err(|_| usage())?;
                let listener = tokio::net::TcpListener::bind(address)
                    .await
                    .map_err(|_| failed("Unable to bind HTTP listener."))?;
                eprintln!(
                    "agent-computer-server listening on {}",
                    listener
                        .local_addr()
                        .map_err(|_| failed("Unable to inspect listener."))?
                );
                let shutdown = async {
                    let mut terminate =
                        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                            .expect("install SIGTERM handler");
                    tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
                };
                axum::serve(listener, router)
                    .with_graceful_shutdown(shutdown)
                    .await
                    .map_err(|_| failed("HTTP service failed."))?;
            }
            "credential-issue" => {
                let organization =
                    OrganizationId::new(required("organization")?).map_err(|_| usage())?;
                let principal = PrincipalId::new(required("principal")?).map_err(|_| usage())?;
                let kind = match required("kind")? {
                    "human" => PrincipalKind::Human,
                    "agent" => PrincipalKind::Agent,
                    _ => return Err(usage()),
                };
                let scopes: Result<Vec<_>, _> = required("scopes")?
                    .split(',')
                    .map(|s| s.parse::<ServiceScope>().map_err(|_| usage()))
                    .collect();
                let seconds = required("ttl-seconds")?
                    .parse::<u64>()
                    .map_err(|_| usage())?;
                if !(1..=86400).contains(&seconds) {
                    return Err(usage());
                }
                let scopes = scopes?;
                // Reserve a new private output file before minting a token. Never
                // overwrite an existing secret or print the bearer to a terminal.
                let output_path = working_path(required("output")?);
                let mut output = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&output_path)
                    .map_err(|_| failed("Unable to create a new private credential file."))?;
                let issued = match store
                    .issue_credential(IssueCredential {
                        organization: &organization,
                        principal: &principal,
                        kind,
                        scopes: &scopes,
                        lifetime: Duration::from_secs(seconds),
                    })
                    .await
                {
                    Ok(issued) => issued,
                    Err(_) => {
                        let _ = fs::remove_file(&output_path);
                        return Err(failed("Credential issuance failed."));
                    }
                };
                if writeln!(output, "{}", issued.expose_token())
                    .and_then(|_| output.sync_all())
                    .is_err()
                {
                    let _ = store.revoke_credential(&organization, issued.id()).await;
                    let _ = fs::remove_file(&output_path);
                    return Err(failed(
                        "Credential output failed; check and revoke unused credentials before retrying.",
                    ));
                }
                println!(
                    "{}",
                    serde_json::json!({"credential_id":issued.id(),"organization":organization.as_str(),"principal":principal.as_str(),"expires_in_seconds":seconds})
                );
            }
            "credential-revoke" | "principal-disable" => {
                let organization =
                    OrganizationId::new(required("organization")?).map_err(|_| usage())?;
                let found = if command == "credential-revoke" {
                    store
                        .revoke_credential(&organization, required("credential")?)
                        .await
                } else {
                    let principal =
                        PrincipalId::new(required("principal")?).map_err(|_| usage())?;
                    store.disable_principal(&organization, &principal).await
                }
                .map_err(|_| failed("Credential administration failed."))?;
                if !found {
                    return Err(failed(
                        "No matching credential or principal in this organization.",
                    ));
                }
                println!("{}", serde_json::json!({"updated":true}));
            }
            _ => unreachable!(),
        }
    }
    pool.close().await;
    Ok(())
}

fn working_path(path: &str) -> PathBuf {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        return path;
    }
    std::env::var_os("BUILD_WORKING_DIRECTORY")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(path)
}

pub(crate) fn private_file(path: &str) -> Result<String, Failure> {
    let path = working_path(path);
    let metadata = fs::symlink_metadata(&path)
        .map_err(|_| failed("Unable to inspect database secret file."))?;
    if !metadata.is_file() || metadata.mode() & 0o077 != 0 {
        return Err(failed(
            "Database secret must be a private regular file (mode 0600 or 0400).",
        ));
    }
    let file = fs::File::open(&path).map_err(|_| failed("Unable to open database secret file."))?;
    let opened = file
        .metadata()
        .map_err(|_| failed("Unable to inspect opened database secret file."))?;
    if opened.ino() != metadata.ino()
        || opened.dev() != metadata.dev()
        || opened.mode() & 0o077 != 0
    {
        return Err(failed("Database secret file changed while opening."));
    }
    let mut value = String::new();
    file.take(8193)
        .read_to_string(&mut value)
        .map_err(|_| failed("Unable to read database secret file."))?;
    if value.len() > 8192 || value.trim().is_empty() {
        return Err(failed("Invalid database secret file length."));
    }
    Ok(value)
}
