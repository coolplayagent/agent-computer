use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    Directory,
    File {
        sha256: String,
        size: u64,
        executable: bool,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub entries: BTreeMap<String, Entry>,
}

impl Manifest {
    pub fn digest(&self) -> Result<String> {
        digest("agent-computer/candidate-manifest-v1", self)
    }
    pub(crate) fn validate(&self, quota: u64) -> Result<BTreeSet<String>> {
        if self.entries.len() > 10_000 {
            return Err(Error::InvalidRequest);
        }
        let mut directories = BTreeSet::new();
        let mut total = 4096u64;
        for (path, entry) in &self.entries {
            if !relative(path) {
                return Err(Error::InvalidRequest);
            }
            if let Entry::File { sha256, size, .. } = entry {
                if !is_digest(sha256) {
                    return Err(Error::InvalidRequest);
                }
                let allocated = size.checked_add(4095).ok_or(Error::InvalidRequest)? / 4096 * 4096;
                total = total.checked_add(allocated).ok_or(Error::InvalidRequest)?;
            } else {
                directories.insert(path.clone());
            }
            let mut prefix = String::new();
            let parts: Vec<_> = path.split('/').collect();
            for part in &parts[..parts.len() - 1] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if matches!(self.entries.get(&prefix), Some(Entry::File { .. })) {
                    return Err(Error::InvalidRequest);
                }
                directories.insert(prefix.clone());
            }
        }
        total = total
            .checked_add((directories.len() as u64) * 4096)
            .ok_or(Error::InvalidRequest)?;
        if total > quota || directories.len() > 10_000 {
            return Err(Error::InvalidRequest);
        }
        Ok(directories)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareRequest {
    pub organization: String,
    pub volume_uid: String,
    pub workspace: String,
    pub candidate: String,
    pub computer: String,
    pub generation: u64,
    pub quota_bytes: u64,
    pub manifest_digest: String,
    pub manifest: Manifest,
}
impl PrepareRequest {
    pub(crate) fn validate(&self) -> Result<BTreeSet<String>> {
        if ![
            &self.organization,
            &self.volume_uid,
            &self.workspace,
            &self.candidate,
            &self.computer,
        ]
        .into_iter()
        .all(|s| identifier(s))
            || self.generation == 0
            || self.generation > i64::MAX as u64
            || self.quota_bytes == 0
            || self.quota_bytes > i64::MAX as u64
            || !self.quota_bytes.is_multiple_of(1 << 30)
            || self.manifest_digest != self.manifest.digest()?
        {
            return Err(Error::InvalidRequest);
        }
        self.manifest.validate(self.quota_bytes)
    }
    pub(crate) fn parent(&self) -> String {
        format!(
            "organization/{}/workspace/{}/candidates/{}/generation",
            self.organization, self.workspace, self.candidate
        )
    }
    pub fn path_ref(&self) -> String {
        format!("{}/{}/data", self.parent(), self.generation)
    }
    /// Stable binding shared by the durable dispatcher and the storage adapter.
    pub fn binding_digest(&self, volume_path: &str, uid: u32, gid: u32) -> Result<String> {
        self.validate()?;
        if !relative(volume_path) || uid == 0 || uid == u32::MAX || gid == 0 || gid == u32::MAX {
            return Err(Error::InvalidRequest);
        }
        digest(
            "agent-computer/candidate-preparation-v1",
            &(self, volume_path, uid, gid),
        )
    }
}

/// Persistent preparation receipt outside the application-mounted data directory.
/// This is storage evidence; callers must independently authorize each writer.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prepared {
    pub version: u32,
    pub request_digest: String,
    pub filesystem_uuid: String,
    pub volume_uid: String,
    pub path_ref: String,
    pub data_inode: u64,
    pub manifest_digest: String,
    pub quota_bytes: u64,
}

pub(crate) fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}
pub(crate) fn relative(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 1024
        && !s.contains(['\\', '\0'])
        && s.split('/').count() <= 32
        && s.split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && p.len() <= 255)
}
pub(crate) fn is_digest(s: &str) -> bool {
    s.len() == 71
        && s.starts_with("sha256:")
        && s[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub(crate) fn digest(domain: &str, value: &impl Serialize) -> Result<String> {
    let mut value = serde_json::to_value(value).map_err(|_| Error::InvalidRequest)?;
    value.sort_all_objects();
    let bytes = serde_json::to_vec(&value).map_err(|_| Error::InvalidRequest)?;
    let mut hash = Sha256::new();
    hash.update(domain);
    hash.update([0]);
    hash.update(bytes);
    Ok(format!("sha256:{:x}", hash.finalize()))
}
