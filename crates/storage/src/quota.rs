//! Synchronous, bounded invocation of an operator-pinned JuiceFS executable.
use crate::{
    Error, Result,
    directory::{Dir, bounded},
    model,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    os::unix::{fs::MetadataExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Implementations must verify the target filesystem and acknowledge the exact
/// directory limit before returning. This trusted interface is not tenant input.
pub trait Quota {
    fn ensure(&self, filesystem_uuid: &str, absolute_path: &str, bytes: u64) -> Result<()>;
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JuiceFsConfig {
    pub executable: PathBuf,
    pub executable_sha256: String,
    /// Credential-free PostgreSQL URL. Password is read from a private file.
    pub metadata_url: String,
    pub password_file: PathBuf,
    pub timeout_seconds: u64,
}
pub struct JuiceFsQuota(JuiceFsConfig);
impl JuiceFsQuota {
    pub fn new(config: JuiceFsConfig) -> Result<Self> {
        let url = url::Url::parse(&config.metadata_url).map_err(|_| Error::InvalidRequest)?;
        if url.scheme() != "postgres"
            || url.host_str().is_none()
            || !model::identifier(url.username())
            || url.password().is_some()
            || url.fragment().is_some()
            || !url.path().strip_prefix('/').is_some_and(model::identifier)
            || url.query_pairs().count() != 1
            || !url.query_pairs().all(|(key, value)| {
                key == "sslmode"
                    && ["verify-full", "verify-ca", "require", "disable"].contains(&value.as_ref())
            })
            || !(1..=120).contains(&config.timeout_seconds)
            || !model::is_digest(&config.executable_sha256)
        {
            return Err(Error::InvalidRequest);
        }
        let mut executable = absolute_file(&config.executable)?;
        if executable.metadata()?.mode() & 0o022 != 0 {
            return Err(Error::InvalidRequest);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65_536];
        loop {
            let size = executable.read(&mut buffer)?;
            if size == 0 {
                break;
            }
            hash.update(&buffer[..size]);
        }
        if format!("sha256:{:x}", hash.finalize()) != config.executable_sha256 {
            return Err(Error::InputMismatch);
        }
        read_private(&config.password_file, 8192)?;
        Ok(Self(config))
    }
    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        let mut child = Command::new(&self.0.executable)
            .args(args)
            .process_group(0)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("META_PASSWORD_FILE", &self.0.password_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|_| Error::QuotaUnavailable)?;
        let deadline = Instant::now() + Duration::from_secs(self.0.timeout_seconds);
        let result = (|| -> Result<Vec<u8>> {
            let mut stdout = child.stdout.take().ok_or(Error::QuotaUnavailable)?;
            rustix::fs::fcntl_setfl(&stdout, rustix::fs::OFlags::NONBLOCK)
                .map_err(|_| Error::QuotaUnavailable)?;
            let mut output = Vec::new();
            let mut buffer = [0u8; 8192];
            let mut eof = false;
            loop {
                if Instant::now() >= deadline {
                    return Err(Error::QuotaUnavailable);
                }
                let idle = match stdout.read(&mut buffer) {
                    Ok(0) => {
                        eof = true;
                        true
                    }
                    Ok(size) => {
                        if output.len() + size > 1_048_576 {
                            return Err(Error::QuotaUnavailable);
                        }
                        output.extend_from_slice(&buffer[..size]);
                        false
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => true,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => false,
                    Err(_) => return Err(Error::QuotaUnavailable),
                };
                if let Some(status) = child.try_wait().map_err(|_| Error::QuotaUnavailable)? {
                    if !status.success() {
                        return Err(Error::QuotaUnavailable);
                    }
                    if eof {
                        return Ok(output);
                    }
                }
                // A child that exits while a descendant holds stdout must not
                // bypass the same deadline that bounds the command itself.
                if idle {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        })();
        if result.is_err() {
            if let Some(pid) = rustix::process::Pid::from_raw(child.id() as i32) {
                let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }
}
impl Quota for JuiceFsQuota {
    fn ensure(&self, uuid: &str, path: &str, bytes: u64) -> Result<()> {
        if !model::identifier(uuid)
            || !path.strip_prefix('/').is_some_and(model::relative)
            || bytes == 0
            || bytes > i64::MAX as u64
            || !bytes.is_multiple_of(1 << 30)
        {
            return Err(Error::InvalidRequest);
        }
        let status: serde_json::Value =
            serde_json::from_slice(&self.run(&["status", &self.0.metadata_url])?)
                .map_err(|_| Error::QuotaUnavailable)?;
        if status.pointer("/Setting/UUID").and_then(|v| v.as_str()) != Some(uuid) {
            return Err(Error::IdentityConflict);
        }
        self.run(&[
            "quota",
            "set",
            "--path",
            path,
            "--capacity",
            &(bytes >> 30).to_string(),
            &self.0.metadata_url,
        ])?;
        Ok(())
    }
}
fn absolute_file(path: &Path) -> Result<std::fs::File> {
    if !path.is_absolute() {
        return Err(Error::InvalidRequest);
    }
    let parent = Dir::root(path.parent().ok_or(Error::InvalidRequest)?)?;
    parent.file(
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or(Error::InvalidRequest)?,
    )
}
/// Operator-owned regular file; callers must also protect its ancestor directories.
pub fn read_private(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = absolute_file(path)?;
    let metadata = file.metadata()?;
    if metadata.mode() & 0o077 != 0 || metadata.uid() != rustix::process::geteuid().as_raw() {
        return Err(Error::InvalidRequest);
    }
    bounded(file, limit)
}
