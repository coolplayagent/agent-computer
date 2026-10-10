use super::{Failure, Result, local_path};
use reqwest::header::HeaderMap;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::Write;

pub(super) const MAX_BYTES: usize = 1024 * 1024;

pub(super) fn save(path: &str, headers: &HeaderMap, bytes: &[u8]) -> Result<Value> {
    let bad = || {
        Failure::local(
            "invalid_output",
            "Output bytes or metadata failed verification; no file was published.",
        )
    };
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(bad)
    };
    let sha256 = format!("sha256:{:x}", Sha256::digest(bytes));
    let manifest = header("x-output-manifest-digest")?;
    let observed: u64 = header("x-output-observed-bytes")?
        .parse()
        .map_err(|_| bad())?;
    let truncated: bool = header("x-output-truncated")?.parse().map_err(|_| bad())?;
    let eof: bool = header("x-output-eof")?.parse().map_err(|_| bad())?;
    let length: usize = header("content-length")?.parse().map_err(|_| bad())?;
    if length != bytes.len()
        || sha256 != header("x-output-sha256")?
        || observed < length as u64
        || truncated != (observed > length as u64)
        || agent_computer_core::identity::InputDigest::parse(manifest).is_err()
        || header("content-type")? != "application/octet-stream"
    {
        return Err(bad());
    }
    let target = local_path(path);
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let unavailable = || {
        Failure::local(
            "output_unavailable",
            "Unable to publish a new output file; existing files are never replaced.",
        )
    };
    let mut file = tempfile::NamedTempFile::new_in(parent).map_err(|_| unavailable())?;
    file.write_all(bytes)
        .and_then(|()| file.as_file().sync_all())
        .map_err(|_| unavailable())?;
    file.persist_noclobber(&target).map_err(|_| unavailable())?;
    Ok(
        json!({"path":path,"bytes":bytes.len(),"sha256":sha256,"manifest_digest":manifest,
        "observed_bytes":observed,"truncated":truncated,"eof":eof}),
    )
}
