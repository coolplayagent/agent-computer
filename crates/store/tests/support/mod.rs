use agent_computer_core::identity::{IdempotencyKey, OrganizationId, PrincipalId};
use agent_computer_definitions::{Format, ValidatedDefinition, validate_bytes};
use agent_computer_store::{Precondition, Receipt, RecordDeclaration, Store};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Duration,
};

pub struct Database {
    pub store: Store,
    pub pool: PgPool,
    child: Option<Child>,
    bin: PathBuf,
    // Drop stops PostgreSQL before the temporary directory is removed.
    directory: tempfile::TempDir,
}

impl Database {
    pub async fn new() -> Self {
        let bin = std::env::var_os("AGENT_COMPUTER_PG_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/usr/lib/postgresql/18/bin"));
        assert!(
            bin.join("initdb").is_file(),
            "Install PostgreSQL 18 or set AGENT_COMPUTER_PG_BIN; database tests must not be skipped"
        );
        // A short private socket path also works inside Bazel's long sandbox paths.
        let directory = tempfile::Builder::new()
            .prefix("ac-pg-")
            .tempdir_in("/tmp")
            .unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut init = Command::new(bin.join("initdb"));
        init.arg("-D").arg(directory.path().join("data")).args([
            "--no-locale",
            "--encoding=UTF8",
            "--auth-local=trust",
            "--auth-host=reject",
            "--username=store_test",
        ]);
        if let Some(share) = std::env::var_os("AGENT_COMPUTER_PG_SHARE") {
            init.arg("-L").arg(share);
        }
        let output = init.output().expect("start initdb");
        assert!(
            output.status.success(),
            "initdb: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let options = Self::options(directory.path());
        let pool = PgPoolOptions::new()
            .max_connections(24)
            .acquire_timeout(Duration::from_secs(10))
            .connect_lazy_with(options);
        let mut db = Self {
            store: Store::new(pool.clone()),
            pool,
            child: None,
            bin,
            directory,
        };
        db.start().await;
        // Exercise migration locking on every fresh cluster.
        let (first, second) = tokio::join!(db.store.migrate(), db.store.migrate());
        first.unwrap();
        second.unwrap();
        db
    }

    fn options(path: &Path) -> PgConnectOptions {
        PgConnectOptions::new()
            .host(path.to_str().unwrap())
            .port(5432)
            .ssl_mode(sqlx::postgres::PgSslMode::Disable)
            .username("store_test")
            .database("postgres")
            .application_name("agent_computer_store_tests")
    }

    async fn start(&mut self) {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.path().join("postgres.log"))
            .unwrap();
        self.child = Some(
            Command::new(self.bin.join("postgres"))
                .arg("-D")
                .arg(self.directory.path().join("data"))
                .arg("-k")
                .arg(self.directory.path())
                .args([
                    "-h",
                    "",
                    "-c",
                    "shared_buffers=16MB",
                    "-c",
                    "max_connections=40",
                ])
                .stdout(Stdio::from(log.try_clone().unwrap()))
                .stderr(Stdio::from(log))
                .spawn()
                .unwrap(),
        );
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if self.child.as_mut().unwrap().try_wait().unwrap().is_some() {
                    return false;
                }
                if self.pool.acquire().await.is_ok() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or(false);
        if ready {
            return;
        }
        panic!(
            "PostgreSQL startup failed: {}",
            fs::read_to_string(self.directory.path().join("postgres.log")).unwrap()
        );
    }

    pub async fn crash_and_restart(&mut self) {
        self.pool.close().await;
        self.stop("immediate"); // PostgreSQL performs WAL crash recovery on restart.
        self.pool = PgPoolOptions::new()
            .max_connections(24)
            .connect_lazy_with(Self::options(self.directory.path()));
        self.store = Store::new(self.pool.clone());
        self.start().await;
    }

    fn stop(&mut self, mode: &str) {
        if let Some(mut child) = self.child.take() {
            let result = Command::new(self.bin.join("pg_ctl"))
                .arg("-D")
                .arg(self.directory.path().join("data"))
                .args(["stop", "-m", mode, "-w", "-t", "10"])
                .output();
            if !result.is_ok_and(|output| output.status.success()) {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        self.stop("fast");
    }
}

pub fn org(value: &str) -> OrganizationId {
    OrganizationId::new(value).unwrap()
}
pub fn principal(value: &str) -> PrincipalId {
    PrincipalId::new(value).unwrap()
}
pub fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::new(value).unwrap()
}

pub fn definition(name: &str, expected: Option<u64>) -> ValidatedDefinition {
    let mut document = serde_json::json!({"apiVersion":"agent-computer/v1alpha1", "kind":"ComputerSet", "metadata":{"name":name}, "spec":{"agents":[{"name":"external", "mode":"external", "adapter":"tools-api", "capabilities":[]}]}});
    if let Some(expected) = expected {
        document["metadata"]["expectedRevision"] = expected.into();
    }
    validate_bytes(&serde_json::to_vec(&document).unwrap(), Format::Json).unwrap()
}

pub async fn record(
    store: &Store,
    organization: &str,
    actor: &str,
    request_key: &str,
    name: &str,
    precondition: Precondition,
) -> agent_computer_store::Result<Receipt> {
    store
        .record(RecordDeclaration {
            organization: &org(organization),
            principal: &principal(actor),
            key: &key(request_key),
            precondition,
            definition: &definition(name, None),
        })
        .await
}
