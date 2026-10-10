//! Chunked immutable Workspace artifacts; every publication is read back in full.
use crate::{Client, Error, ObjectRef, Result, Spool, sha256};
use agent_computer_storage::{
    Entry, Manifest, MountedVolume, Prepared,
    capture::{CHUNK_BYTES, CaptureSink, Chunk},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Bundle {
    pub version: u32,
    pub organization: String,
    pub store_digest: String,
    pub commit_id: String,
    pub prepared: Prepared,
    pub manifest: Manifest,
    pub chunks: BTreeMap<String, Vec<ObjectRef>>,
}
impl Bundle {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        if bytes.len() > crate::MAX_BYTES {
            return Err(Error::Limit);
        }
        Ok(bytes)
    }
    pub fn validate(&self) -> Result<()> {
        if self.version != 1
            || !crate::digest(&self.store_digest)
            || !crate::identifier(&self.organization)
            || !crate::identifier(&self.commit_id)
        {
            return Err(Error::Invalid);
        }
        self.manifest
            .validate(self.prepared.quota_bytes)
            .map_err(|_| Error::Invalid)?;
        if self.chunks.len()
            != self
                .manifest
                .entries
                .values()
                .filter(|e| matches!(e, Entry::File { .. }))
                .count()
        {
            return Err(Error::Invalid);
        }
        for (path, entry) in &self.manifest.entries {
            if let Entry::File { size, .. } = entry {
                let chunks = self.chunks.get(path).ok_or(Error::Invalid)?;
                let mut total = 0u64;
                for (i, chunk) in chunks.iter().enumerate() {
                    chunk.validate()?;
                    if chunk.store_digest != self.store_digest
                        || chunk.key
                            != format!(
                                "artifacts/v1/{}/{}/{}",
                                self.organization,
                                self.commit_id,
                                &chunk.sha256[7..]
                            )
                        || chunk.size == 0
                        || chunk.size > CHUNK_BYTES as u64
                        || i + 1 < chunks.len() && chunk.size != CHUNK_BYTES as u64
                    {
                        return Err(Error::Invalid);
                    }
                    total = total.checked_add(chunk.size).ok_or(Error::Limit)?;
                }
                if total != *size {
                    return Err(Error::Integrity);
                }
            }
        }
        Ok(())
    }
    pub fn object(&self, client: &Client) -> Result<ObjectRef> {
        if client.store_digest() != self.store_digest {
            return Err(Error::Integrity);
        }
        let bytes = self.bytes()?;
        client.artifact_reference(
            &self.organization,
            &self.commit_id,
            &sha256(&bytes),
            bytes.len() as u64,
        )
    }
}
/// Capture evidence cannot be deserialized from caller input.
pub struct CapturedArtifact(Bundle);
impl CapturedArtifact {
    pub fn bundle(&self) -> &Bundle {
        &self.0
    }
    pub fn capture(
        client: &Client,
        spool: &Spool,
        mount: &MountedVolume,
        prepared: &Prepared,
        organization: &str,
        commit: &str,
    ) -> Result<Self> {
        struct Sink<'a> {
            client: &'a Client,
            spool: &'a Spool,
            org: &'a str,
            commit: &'a str,
        }
        impl CaptureSink for Sink<'_> {
            fn record(
                &mut self,
                chunk: &Chunk,
                bytes: &[u8],
            ) -> agent_computer_storage::Result<()> {
                let reference = self
                    .client
                    .artifact_reference(self.org, self.commit, &chunk.sha256, chunk.size)
                    .map_err(|_| agent_computer_storage::Error::Io)?;
                self.spool
                    .record(&chunk.sha256, &[(reference, bytes.to_vec())])
                    .map_err(|_| agent_computer_storage::Error::Io)
            }
        }
        let tree = mount
            .capture(
                prepared,
                &mut Sink {
                    client,
                    spool,
                    org: organization,
                    commit,
                },
            )
            .map_err(|_| Error::Io)?;
        let chunks = tree
            .chunks()
            .iter()
            .map(|(p, chunks)| {
                Ok((
                    p.clone(),
                    chunks
                        .iter()
                        .map(|c| client.artifact_reference(organization, commit, &c.sha256, c.size))
                        .collect::<Result<Vec<_>>>()?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        let bundle = Bundle {
            version: 1,
            store_digest: client.store_digest().into(),
            organization: organization.into(),
            commit_id: commit.into(),
            prepared: tree.prepared().clone(),
            manifest: tree.manifest().clone(),
            chunks,
        };
        let bytes = bundle.bytes()?;
        let object = bundle.object(client)?;
        spool.record(&object.sha256, &[(object.clone(), bytes)])?;
        Ok(Self(bundle))
    }
}
/// All referenced chunks, concatenated file hashes and the manifest object were
/// just read back. This is integrity evidence, not business-content approval.
pub struct VerifiedArtifact {
    bundle: Bundle,
    object: ObjectRef,
}
impl VerifiedArtifact {
    pub fn bundle(&self) -> &Bundle {
        &self.bundle
    }
    pub fn object(&self) -> &ObjectRef {
        &self.object
    }
}
async fn get_or_upload(
    client: &Client,
    spool: Option<&Spool>,
    object: &ObjectRef,
) -> Result<Vec<u8>> {
    match client.get(object).await {
        Ok(bytes) => Ok(bytes),
        Err(Error::Missing) if spool.is_some() => {
            let bytes = spool.unwrap().read(&object.sha256, object)?;
            client.put_verified(object, &bytes).await?;
            Ok(bytes)
        }
        Err(error) => Err(error),
    }
}
pub async fn verify(
    client: &Client,
    bundle: &Bundle,
    spool: Option<&Spool>,
) -> Result<VerifiedArtifact> {
    bundle.validate()?;
    for (path, chunks) in &bundle.chunks {
        let mut hash = Sha256::new();
        for chunk in chunks {
            hash.update(get_or_upload(client, spool, chunk).await?);
        }
        let Entry::File {
            sha256: expected, ..
        } = &bundle.manifest.entries[path]
        else {
            return Err(Error::Invalid);
        };
        if format!("sha256:{:x}", hash.finalize()) != *expected {
            return Err(Error::Integrity);
        }
    }
    let object = bundle.object(client)?;
    if get_or_upload(client, spool, &object).await? != bundle.bytes()? {
        return Err(Error::Integrity);
    }
    Ok(VerifiedArtifact {
        bundle: bundle.clone(),
        object,
    })
}

/// Restore verified chunks to a private, digest-addressed cache. Materialization
/// rechecks the concatenated file hash and creates independent writable inodes.
pub async fn restore(
    client: &Client,
    bundle: &Bundle,
    cache: &agent_computer_storage::ObjectCache,
) -> Result<()> {
    verify(client, bundle, None).await?;
    for (path, chunks) in &bundle.chunks {
        let Entry::File {
            sha256: hash, size, ..
        } = &bundle.manifest.entries[path]
        else {
            return Err(Error::Invalid);
        };
        let mut writer = cache.begin_file(hash, *size).map_err(|_| Error::Io)?;
        for chunk in chunks {
            use std::io::Write;
            writer
                .write_all(&client.get(chunk).await?)
                .map_err(|_| Error::Io)?;
        }
        writer.finish().map_err(|_| Error::Integrity)?;
    }
    Ok(())
}
