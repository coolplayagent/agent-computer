//! Immutable chunk publication. This authority can retain observations, never run a command.
mod read;
use super::*;
use agent_computer_kubernetes::OutputChunkObservation;
use agent_computer_sandbox::streaming::{CHUNK_BYTES, Chunk, MAX_CHUNKS, Progress, Stream};
use std::sync::{Arc, OnceLock};

async fn spool_io<T: Send + 'static>(
    f: impl FnOnce() -> agent_computer_objects::Result<T> + Send + 'static,
) -> Result<T> {
    static SLOTS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(4)))
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| Error::RuntimeAccessUnavailable)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        f()
    })
    .await
    .map_err(|_| Error::RuntimeAccessUnavailable)?
    .map_err(|_| Error::ExecutionOutputUnavailable)
}
pub use read::{ExecutionChunkDownload, ExecutionChunkPage};

pub struct ExecutionOutputCapture {
    dispatch: ExecutionDispatchIntent,
}
impl ExecutionDispatchAttempt {
    /// Derive a separate bounded publication handle while the original attempt
    /// is live, so object IO never holds up renewal of that attempt.
    pub fn output_capture(&self) -> Result<Option<ExecutionOutputCapture>> {
        self.remaining_budget_ms()?;
        Ok(self
            .intent()
            .execution
            .stream_output
            .then(|| ExecutionOutputCapture {
                dispatch: self.intent().clone(),
            }))
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkManifest {
    version: u8,
    organization: String,
    execution_id: String,
    pod_uid: String,
    dispatch_digest: String,
    grant_digest: String,
    arm_digest: String,
    sequence: u32,
    previous_digest: String,
    chunk_digest: String,
    stream: Stream,
    offset: u64,
    observed_bytes: u64,
    truncated: bool,
    eof: bool,
    object: ObjectRef,
}
impl ChunkManifest {
    fn digest(&self) -> Result<String> {
        digest("agent-computer/execution-output-chunk-manifest-v1", self)
    }
    fn validate(&self) -> Result<()> {
        self.object
            .validate()
            .map_err(|_| Error::InvalidStoredData)?;
        if self.version != 1
            || !agent_computer_objects::identifier(&self.organization)
            || !agent_computer_objects::identifier(&self.execution_id)
            || !agent_computer_objects::identifier(&self.pod_uid)
            || !(1..=MAX_CHUNKS).contains(&self.sequence)
            || self.object.size > CHUNK_BYTES as u64
            || self.offset > agent_computer_sandbox::MAX_OUTPUT_BYTES as u64
            || self.offset.checked_add(self.object.size).is_none_or(|n| {
                n > self.observed_bytes || n > agent_computer_sandbox::MAX_OUTPUT_BYTES as u64
            })
            || (self.object.size == 0 && !self.eof)
            || [
                &self.dispatch_digest,
                &self.grant_digest,
                &self.arm_digest,
                &self.previous_digest,
                &self.chunk_digest,
            ]
            .into_iter()
            .any(|s| !agent_computer_objects::digest(s))
            || self.object.key
                != format!(
                    "execution-outputs/v1/{}/{}/{}",
                    self.organization,
                    self.execution_id,
                    &self.object.sha256[7..]
                )
        {
            return Err(Error::InvalidStoredData);
        }
        Ok(())
    }
    fn verify_bytes(&self, bytes: Vec<u8>) -> Result<Vec<u8>> {
        self.object
            .verify(&bytes)
            .map_err(|_| Error::ExecutionOutputUnavailable)?;
        let chunk = Chunk {
            version: 1,
            startup_grant_digest: self.grant_digest.clone(),
            sequence: self.sequence,
            previous_digest: self.previous_digest.clone(),
            stream: self.stream,
            offset: self.offset,
            bytes,
            observed_bytes: self.observed_bytes,
            truncated: self.truncated,
            eof: self.eof,
        };
        if chunk.digest().map_err(|_| Error::InvalidStoredData)? != self.chunk_digest {
            return Err(Error::InvalidStoredData);
        }
        Ok(chunk.bytes)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ExecutionOutputChunk {
    pub sequence: u32,
    pub stream: OutputStream,
    pub offset: u64,
    pub retained_bytes: u64,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub eof: bool,
    pub sha256: String,
    pub chunk_digest: String,
    pub verified_at_ms: Option<i64>,
}
fn view(m: &ChunkManifest, verified: Option<i64>) -> ExecutionOutputChunk {
    ExecutionOutputChunk {
        sequence: m.sequence,
        stream: match m.stream {
            Stream::Stdout => OutputStream::Stdout,
            Stream::Stderr => OutputStream::Stderr,
        },
        offset: m.offset,
        retained_bytes: m.object.size,
        observed_bytes: m.observed_bytes,
        truncated: m.truncated,
        eof: m.eof,
        sha256: m.object.sha256.clone(),
        chunk_digest: m.chunk_digest.clone(),
        verified_at_ms: verified,
    }
}
fn decode(row: PgRow, org: &str, id: &str) -> Result<(ChunkManifest, Option<i64>)> {
    let m: ChunkManifest =
        serde_json::from_value(row.try_get("manifest")?).map_err(|_| Error::InvalidStoredData)?;
    m.validate()?;
    if m.organization != org
        || m.execution_id != id
        || m.sequence != row.try_get::<i32, _>("sequence")? as u32
        || m.digest()? != row.try_get::<String, _>("manifest_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok((m, row.try_get("verified_at_ms")?))
}
async fn load(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    sequence: u32,
) -> Result<Option<(ChunkManifest, Option<i64>)>> {
    sqlx::query("SELECT i.sequence,i.manifest,i.manifest_digest,c.verified_at_ms FROM execution_output_chunk_intents i LEFT JOIN execution_output_chunks c USING(organization,execution_id,sequence) WHERE i.organization=$1 AND i.execution_id=$2 AND i.sequence=$3")
        .bind(org).bind(id).bind(sequence as i32).fetch_optional(&mut **tx).await?.map(|r|decode(r,org,id)).transpose()
}
pub(super) async fn progress(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    grant: &ExecutionStartupGrant,
) -> Result<Option<Progress>> {
    if grant.grant.version != agent_computer_sandbox::STREAMING_PROTOCOL {
        return Ok(None);
    }
    let latest:Option<i32>=sqlx::query_scalar("SELECT max(sequence) FROM execution_output_chunk_intents WHERE organization=$1 AND execution_id=$2").bind(org).bind(id).fetch_one(&mut **tx).await?;
    let value = if let Some(sequence) = latest {
        let (m, verified) = load(tx, org, id, sequence as u32)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        if verified.is_none() {
            return Err(Error::ExecutionOutputUnavailable);
        }
        Progress {
            sequence: m.sequence,
            last_digest: m.chunk_digest,
        }
    } else {
        Progress {
            sequence: 0,
            last_digest: grant.grant_digest.clone(),
        }
    };
    Ok(Some(value))
}

impl Store {
    /// Only this opaque observation and original live-derived capture can create
    /// a new pending chunk. SQL stores references/counts, never command output.
    pub async fn collect_candidate_execution_chunk(
        &self,
        capture: &ExecutionOutputCapture,
        observation: &OutputChunkObservation,
        client: &Client,
        spool: &Spool,
    ) -> Result<ExecutionOutputChunk> {
        let original = &capture.dispatch;
        let org = &original.organization;
        let id = &original.execution.execution_id;
        let chunk = observation.chunk();
        chunk
            .validate()
            .map_err(|_| Error::InvalidReconcileResult)?;
        let object = client
            .reference(org, id, &chunk.bytes)
            .map_err(|_| Error::InvalidReconcileResult)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let r = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &r).await?;
        let grant = startup::receipt(&mut tx, org, id)
            .await?
            .ok_or(Error::InvalidReconcileResult)?;
        let arm = watchdogs::receipt(&mut tx, &dispatch)
            .await?
            .ok_or(Error::InvalidReconcileResult)?;
        if original.intent_digest != dispatch.intent_digest
            || !dispatch.execution.stream_output
            || grant.grant.version != agent_computer_sandbox::STREAMING_PROTOCOL
            || observation.pod_uid() != grant.pod_uid
            || chunk.startup_grant_digest != grant.grant_digest
        {
            return Err(Error::InvalidReconcileResult);
        }
        let m = ChunkManifest {
            version: 1,
            organization: org.clone(),
            execution_id: id.clone(),
            pod_uid: grant.pod_uid,
            dispatch_digest: dispatch.intent_digest,
            grant_digest: grant.grant_digest,
            arm_digest: arm.evidence_digest,
            sequence: chunk.sequence,
            previous_digest: chunk.previous_digest.clone(),
            chunk_digest: chunk.digest().map_err(|_| Error::InvalidReconcileResult)?,
            stream: chunk.stream,
            offset: chunk.offset,
            observed_bytes: chunk.observed_bytes,
            truncated: chunk.truncated,
            eof: chunk.eof,
            object,
        };
        m.validate()?;
        if let Some((existing, _)) = load(&mut tx, org, id, m.sequence).await? {
            if existing != m {
                return Err(Error::IdempotencyConflict);
            }
        } else {
            sqlx::query("INSERT INTO execution_output_chunk_intents (organization,execution_id,sequence,manifest,manifest_digest,created_at_ms) VALUES ($1,$2,$3,$4,$5,$6)").bind(org).bind(id).bind(m.sequence as i32).bind(serde_json::to_value(&m).map_err(|_|Error::InvalidStoredData)?).bind(m.digest()?).bind(transactions::now(&mut tx).await?).execute(&mut *tx).await?;
            transactions::emit(&mut tx,org,seq,"execution.output_chunk_pending",serde_json::json!({"execution_id":id,"sequence":m.sequence,"chunk_digest":m.chunk_digest})).await?;
        }
        tx.commit().await?;
        let local = spool.clone();
        let binding = m.digest()?;
        let data = vec![(m.object.clone(), chunk.bytes.clone())];
        spool_io(move || local.record(&binding, &data)).await?;
        self.recover_candidate_execution_chunk(
            &OrganizationId::new(org).map_err(|_| Error::InvalidStoredData)?,
            id,
            m.sequence,
            client,
            spool,
        )
        .await
    }
    /// Retry exactly one durable publication; never reconnect, launch or renew.
    pub async fn recover_candidate_execution_chunk(
        &self,
        org: &OrganizationId,
        id: &str,
        sequence: u32,
        client: &Client,
        spool: &Spool,
    ) -> Result<ExecutionOutputChunk> {
        valid_id(id)?;
        if !(1..=MAX_CHUNKS).contains(&sequence) {
            return Err(Error::InvalidRuntimeRequest);
        }
        let mut tx = self.pool.begin().await?;
        let (m, _) = load(&mut tx, org.as_str(), id, sequence)
            .await?
            .ok_or(Error::ExecutionOutputUnavailable)?;
        tx.commit().await?;
        if m.object.store_digest != client.store_digest() {
            return Err(Error::InvalidReconcileResult);
        }
        let bytes = match client.get(&m.object).await {
            Ok(bytes) => bytes,
            Err(agent_computer_objects::Error::Missing) => {
                let local = spool.clone();
                let binding = m.digest()?;
                let object = m.object.clone();
                let bytes = spool_io(move || local.read(&binding, &object)).await?;
                m.verify_bytes(bytes.clone())?;
                let verified = client
                    .put_verified(&m.object, &bytes)
                    .await
                    .map_err(|_| Error::ExecutionOutputUnavailable)?;
                if verified.reference() != &m.object {
                    return Err(Error::InvalidReconcileResult);
                }
                bytes
            }
            Err(_) => return Err(Error::ExecutionOutputUnavailable),
        };
        m.verify_bytes(bytes)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let (current, verified) = load(&mut tx, org.as_str(), id, sequence)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        if current != m {
            return Err(Error::IdempotencyConflict);
        }
        let at = if let Some(at) = verified {
            at
        } else {
            let at = transactions::now(&mut tx).await?;
            sqlx::query("INSERT INTO execution_output_chunks (organization,execution_id,sequence,manifest_digest,verified_at_ms) VALUES ($1,$2,$3,$4,$5)").bind(org.as_str()).bind(id).bind(sequence as i32).bind(m.digest()?).bind(at).execute(&mut *tx).await?;
            transactions::emit(&mut tx,org.as_str(),seq,"execution.output_chunk_verified",serde_json::json!({"execution_id":id,"sequence":sequence,"chunk_digest":m.chunk_digest})).await?;
            at
        };
        tx.commit().await?;
        Ok(view(&m, Some(at)))
    }
    /// The serial publication invariant permits at most one pending chunk.
    pub async fn recover_candidate_execution_chunks(
        &self,
        org: &OrganizationId,
        id: &str,
        client: &Client,
        spool: &Spool,
    ) -> Result<Option<ExecutionOutputChunk>> {
        valid_id(id)?;
        let sequence:Option<i32>=sqlx::query_scalar("SELECT i.sequence FROM execution_output_chunk_intents i LEFT JOIN execution_output_chunks c USING(organization,execution_id,sequence) WHERE i.organization=$1 AND i.execution_id=$2 AND c.sequence IS NULL ORDER BY i.sequence LIMIT 1").bind(org.as_str()).bind(id).fetch_optional(&self.pool).await?;
        match sequence {
            Some(n) => self
                .recover_candidate_execution_chunk(org, id, n as u32, client, spool)
                .await
                .map(Some),
            None => Ok(None),
        }
    }
}
