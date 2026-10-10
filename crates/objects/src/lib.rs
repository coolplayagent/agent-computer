//! Bounded content-addressed output objects. No execution or fencing authority.
#![forbid(unsafe_code)]
mod s3;
mod spool;
pub use s3::{Client, Configuration};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub use spool::Spool;

pub const MAX_BYTES: usize = 8 * 1024 * 1024 + 16384;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectRef {
    pub store_digest: String,
    pub key: String,
    pub sha256: String,
    pub size: u64,
}
impl ObjectRef {
    pub fn validate(&self) -> Result<()> {
        let parts: Vec<_> = self.key.split('/').collect();
        if !digest(&self.store_digest)
            || !digest(&self.sha256)
            || self.size > MAX_BYTES as u64
            || self.key.len() > 512
            || parts.len() != 5
            || parts[0] != "execution-outputs"
            || parts[1] != "v1"
            || !parts.iter().all(|s| identifier(s))
            || parts[4] != &self.sha256[7..]
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
    pub fn verify(&self, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        if bytes.len() as u64 != self.size || sha256(bytes) != self.sha256 {
            return Err(Error::Integrity);
        }
        Ok(())
    }
}

/// Constructed only after a complete authenticated GET matches the expected bytes.
pub struct VerifiedObject(ObjectRef);
impl VerifiedObject {
    pub fn reference(&self) -> &ObjectRef {
        &self.0
    }
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
pub fn digest(s: &str) -> bool {
    s.len() == 71
        && s.starts_with("sha256:")
        && s[7..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Error {
    Invalid,
    Configuration,
    Transport,
    Rejected,
    Missing,
    Integrity,
    Limit,
    Io,
}
pub type Result<T> = std::result::Result<T, Error>;
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "output object: {self:?}")
    }
}
impl std::error::Error for Error {}

#[cfg(test)]
mod tests;
