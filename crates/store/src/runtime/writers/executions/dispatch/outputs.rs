//! Publication of observations only. No terminal execution or writer transition.
mod chunks;
mod download;
use super::*;
use agent_computer_kubernetes::StartupObservation;
use agent_computer_objects::{Client, ObjectRef, Spool};
use agent_computer_sandbox::{Outcome, Output, StartupReport};
pub use chunks::{
    ExecutionChunkDownload, ExecutionChunkPage, ExecutionOutputCapture, ExecutionOutputChunk,
};
pub use download::{ExecutionOutputDownload, OutputStream};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamMetadata {
    pub sha256: String,
    pub retained_bytes: u64,
    pub observed_bytes: u64,
    pub truncated: bool,
    pub eof: bool,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Summary {
    observed_outcome: Outcome,
    stdout: StreamMetadata,
    stderr: StreamMetadata,
    supervisor_stderr_bytes: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u8,
    organization: String,
    execution_id: String,
    pod_uid: String,
    dispatch_digest: String,
    grant_digest: String,
    arm_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renewal: Option<agent_computer_sandbox::renewal::Progress>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stream: Option<agent_computer_sandbox::streaming::Progress>,
    // Fixed order: envelope, stdout, stderr, supervisor diagnostics.
    objects: [ObjectRef; 4],
    summary: Summary,
}
impl Manifest {
    fn digest(&self) -> Result<String> {
        digest("agent-computer/execution-outputs-v1", self)
    }
    fn validate(&self) -> Result<()> {
        if !matches!(
            (self.version, self.renewal.is_some(), self.stream.is_some()),
            (1, false, false) | (2, true, false) | (3, _, true)
        ) || !agent_computer_objects::identifier(&self.organization)
            || !agent_computer_objects::identifier(&self.execution_id)
            || self.pod_uid.is_empty()
            || [&self.dispatch_digest, &self.grant_digest, &self.arm_digest]
                .into_iter()
                .any(|s| !agent_computer_objects::digest(s))
        {
            return Err(Error::InvalidStoredData);
        }
        if let Some(progress) = &self.renewal {
            progress.validate().map_err(|_| Error::InvalidStoredData)?;
        }
        if let Some(progress) = &self.stream {
            progress.validate().map_err(|_| Error::InvalidStoredData)?;
        }
        for object in &self.objects {
            object.validate().map_err(|_| Error::InvalidStoredData)?;
            if object.store_digest != self.objects[0].store_digest
                || object.key
                    != format!(
                        "execution-outputs/v1/{}/{}/{}",
                        self.organization,
                        self.execution_id,
                        &object.sha256[7..]
                    )
            {
                return Err(Error::InvalidStoredData);
            }
        }
        for (m, o) in [
            (&self.summary.stdout, &self.objects[1]),
            (&self.summary.stderr, &self.objects[2]),
        ] {
            if m.sha256 != o.sha256
                || m.retained_bytes != o.size
                || m.observed_bytes < m.retained_bytes
                || m.truncated != (m.observed_bytes > m.retained_bytes)
            {
                return Err(Error::InvalidStoredData);
            }
        }
        if self.summary.supervisor_stderr_bytes != self.objects[3].size
            || self.objects[3].size > 65536
        {
            return Err(Error::InvalidStoredData);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputState {
    Pending,
    Verified,
}
/// No bucket, key, credential, user output bytes or accepted-completion claim.
#[derive(Debug, Serialize)]
pub struct ExecutionOutput {
    pub execution_id: String,
    pub manifest_digest: String,
    pub state: OutputState,
    pub observed_outcome: Outcome,
    pub stdout: StreamMetadata,
    pub stderr: StreamMetadata,
    pub verified_at_ms: Option<i64>,
}
fn view(m: &Manifest, verified: Option<i64>) -> Result<ExecutionOutput> {
    Ok(ExecutionOutput {
        execution_id: m.execution_id.clone(),
        manifest_digest: m.digest()?,
        state: if verified.is_some() {
            OutputState::Verified
        } else {
            OutputState::Pending
        },
        observed_outcome: m.summary.observed_outcome,
        stdout: m.summary.stdout.clone(),
        stderr: m.summary.stderr.clone(),
        verified_at_ms: verified,
    })
}
fn stream(output: &Output, cap: usize) -> Result<StreamMetadata> {
    if output.bytes.len() > cap
        || output.observed_bytes < output.bytes.len() as u64
        || output.truncated != (output.observed_bytes > output.bytes.len() as u64)
    {
        return Err(Error::InvalidReconcileResult);
    }
    Ok(StreamMetadata {
        sha256: agent_computer_objects::sha256(&output.bytes),
        retained_bytes: output.bytes.len() as u64,
        observed_bytes: output.observed_bytes,
        truncated: output.truncated,
        eof: output.eof,
    })
}
fn content(
    dispatch: &ExecutionDispatchIntent,
    grant: &ExecutionStartupGrant,
    pod: &str,
    bytes: &[u8],
    diagnostics: &[u8],
    renewal: Option<&agent_computer_sandbox::renewal::Progress>,
    stream_progress: Option<&agent_computer_sandbox::streaming::Progress>,
) -> Result<(Summary, Vec<Vec<u8>>)> {
    let cap = dispatch.input.command.output_limit_bytes;
    if bytes.len() > 8 * cap + 16384 || diagnostics.len() > 65536 {
        return Err(Error::InvalidReconcileResult);
    }
    let parsed: StartupReport =
        serde_json::from_slice(bytes).map_err(|_| Error::InvalidReconcileResult)?;
    let mut request = dispatch.bootstrap()?.request;
    request.lease_budget_ms = grant.grant.lease_budget_ms;
    let r = &parsed.report;
    if parsed.version != grant.grant.version
        || parsed.renewal.as_ref() != renewal
        || parsed.stream.as_ref() != stream_progress
        || r.version != 1
        || r.execution_id != dispatch.execution.execution_id
        || r.generation != dispatch.execution.generation as u64
        || pod != grant.pod_uid
        || parsed.challenge_digest != grant.grant.challenge_digest
        || parsed.grant_digest != grant.grant_digest
        || r.request_digest != request.digest().map_err(|_| Error::InvalidStoredData)?
    {
        return Err(Error::InvalidReconcileResult);
    }
    if r.outcome == Outcome::Succeeded
        && (r.main_exit_code != Some(0)
            || r.main_signal.is_some()
            || !r.children_reaped
            || !r.stdout.eof
            || !r.stderr.eof)
    {
        return Err(Error::InvalidReconcileResult);
    }
    let summary = Summary {
        observed_outcome: r.outcome,
        stdout: stream(&r.stdout, cap)?,
        stderr: stream(&r.stderr, cap)?,
        supervisor_stderr_bytes: diagnostics.len() as u64,
    };
    Ok((
        summary,
        vec![
            bytes.to_vec(),
            parsed.report.stdout.bytes,
            parsed.report.stderr.bytes,
            diagnostics.to_vec(),
        ],
    ))
}
async fn load(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
) -> Result<Option<(Manifest, Option<i64>)>> {
    let row=sqlx::query("SELECT i.manifest,i.manifest_digest,o.verified_at_ms FROM execution_output_intents i LEFT JOIN execution_outputs o USING(organization,execution_id) WHERE i.organization=$1 AND i.execution_id=$2")
        .bind(org).bind(id).fetch_optional(&mut **tx).await?;
    let Some(row) = row else { return Ok(None) };
    let m: Manifest =
        serde_json::from_value(row.try_get("manifest")?).map_err(|_| Error::InvalidStoredData)?;
    m.validate()?;
    if m.organization != org
        || m.execution_id != id
        || m.digest()? != row.try_get::<String, _>("manifest_digest")?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some((m, row.try_get("verified_at_ms")?)))
}

/// Only the already verified immutable publication can support a terminal
/// outcome. The original grant and node arm remain part of that binding.
pub(super) async fn verified_outcome(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
    arm: &ExecutionWatchdogArm,
) -> Result<Option<(String, Outcome)>> {
    let Some((manifest, Some(_))) =
        load(tx, &dispatch.organization, &dispatch.execution.execution_id).await?
    else {
        return Ok(None);
    };
    let grant = startup::receipt(tx, &dispatch.organization, &dispatch.execution.execution_id)
        .await?
        .ok_or(Error::InvalidStoredData)?;
    if manifest.dispatch_digest != dispatch.intent_digest
        || manifest.arm_digest != arm.evidence_digest
        || manifest.grant_digest != grant.grant_digest
        || manifest.pod_uid != grant.pod_uid
        || manifest.renewal != renewal_progress(tx, dispatch, &grant).await?
        || manifest.stream
            != chunks::progress(
                tx,
                &dispatch.organization,
                &dispatch.execution.execution_id,
                &grant,
            )
            .await?
    {
        return Err(Error::InvalidStoredData);
    }
    Ok(Some((
        manifest.digest()?,
        manifest.summary.observed_outcome,
    )))
}
async fn renewal_progress(
    tx: &mut Transaction<'_, Postgres>,
    dispatch: &ExecutionDispatchIntent,
    startup: &ExecutionStartupGrant,
) -> Result<Option<agent_computer_sandbox::renewal::Progress>> {
    if startup.grant.hard_budget_ms.is_none() {
        return Ok(None);
    }
    let ack = super::renewal::latest_ack(tx, dispatch).await?;
    Ok(Some(ack.map_or(
        agent_computer_sandbox::renewal::Progress {
            sequence: 0,
            grant_digest: startup.grant_digest.clone(),
        },
        |a| agent_computer_sandbox::renewal::Progress {
            sequence: a.grant.sequence,
            grant_digest: a.grant.grant_digest,
        },
    )))
}
impl Store {
    /// The live attach observation and original dispatch handle are required for
    /// first capture. A durable intent cannot manufacture report bytes or a grant.
    pub async fn collect_candidate_execution_output(
        &self,
        attempt: &ExecutionDispatchAttempt,
        observation: &StartupObservation,
        client: &Client,
        spool: &Spool,
    ) -> Result<ExecutionOutput> {
        let original = attempt.intent();
        let org = &original.organization;
        let id = &original.execution.execution_id;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org).await?;
        let r = row(&mut tx, org, id).await?;
        let dispatch = intent(&mut tx, org, &r).await?;
        let grant = startup::receipt(&mut tx, org, id)
            .await?
            .ok_or(Error::InvalidReconcileResult)?;
        let arm: String=sqlx::query_scalar("SELECT evidence_digest FROM execution_watchdog_arms WHERE organization=$1 AND execution_id=$2 AND pod_uid=$3").bind(org).bind(id).bind(observation.pod_uid()).fetch_one(&mut *tx).await?;
        if dispatch.intent_digest != original.intent_digest {
            return Err(Error::InvalidReconcileResult);
        }
        let renewal = renewal_progress(&mut tx, &dispatch, &grant).await?;
        let stream = chunks::progress(&mut tx, org, id, &grant).await?;
        let (summary, bytes) = content(
            &dispatch,
            &grant,
            observation.pod_uid(),
            observation.report_bytes(),
            observation.supervisor_stderr(),
            renewal.as_ref(),
            stream.as_ref(),
        )?;
        let objects: Vec<_> = bytes
            .iter()
            .map(|b| {
                client
                    .reference(org, id, b)
                    .map_err(|_| Error::InvalidReconcileResult)
            })
            .collect::<Result<_>>()?;
        let m = Manifest {
            version: grant.grant.version as u8,
            organization: org.clone(),
            execution_id: id.clone(),
            pod_uid: observation.pod_uid().into(),
            dispatch_digest: dispatch.intent_digest,
            grant_digest: grant.grant_digest,
            arm_digest: arm,
            renewal,
            stream,
            objects: objects.try_into().map_err(|_| Error::InvalidStoredData)?,
            summary,
        };
        m.validate()?;
        if let Some((existing, _)) = load(&mut tx, org, id).await? {
            if existing != m {
                return Err(Error::IdempotencyConflict);
            }
        } else {
            sqlx::query("INSERT INTO execution_output_intents (organization,execution_id,pod_uid,dispatch_digest,grant_digest,arm_digest,manifest,manifest_digest,created_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9)")
                .bind(org).bind(id).bind(&m.pod_uid).bind(&m.dispatch_digest).bind(&m.grant_digest).bind(&m.arm_digest).bind(serde_json::to_value(&m).map_err(|_|Error::InvalidStoredData)?).bind(m.digest()?).bind(transactions::now(&mut tx).await?).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                org,
                seq,
                "execution.output_pending",
                serde_json::json!({"execution_id":id,"manifest_digest":m.digest()?}),
            )
            .await?;
        }
        tx.commit().await?;
        let data: Vec<_> = m.objects.iter().cloned().zip(bytes).collect();
        let binding = m.digest()?;
        let local = spool.clone();
        tokio::task::spawn_blocking(move || local.record(&binding, &data))
            .await
            .map_err(|_| Error::RuntimeAccessUnavailable)?
            .map_err(|_| Error::ReferenceUnavailable)?;
        self.recover_candidate_execution_output(
            &OrganizationId::new(org).map_err(|_| Error::InvalidStoredData)?,
            id,
            client,
            spool,
        )
        .await
    }
    /// Retrying an immutable publication only. Never creates/attaches/executes.
    pub async fn recover_candidate_execution_output(
        &self,
        org: &OrganizationId,
        id: &str,
        client: &Client,
        spool: &Spool,
    ) -> Result<ExecutionOutput> {
        valid_id(id)?;
        let mut tx = self.pool.begin().await?;
        let (m, _) = load(&mut tx, org.as_str(), id)
            .await?
            .ok_or(Error::RuntimeAccessUnavailable)?;
        tx.commit().await?;
        if m.objects[0].store_digest != client.store_digest() {
            return Err(Error::InvalidReconcileResult);
        }
        for object in &m.objects {
            // Existing objects are read and checked even on a receipt retry.
            match client.get(object).await {
                Ok(_) => {}
                Err(agent_computer_objects::Error::Missing) => {
                    let local = spool.clone();
                    let binding = m.digest()?;
                    let reference = object.clone();
                    let bytes =
                        tokio::task::spawn_blocking(move || local.read(&binding, &reference))
                            .await
                            .map_err(|_| Error::RuntimeAccessUnavailable)?
                            .map_err(|_| Error::ReferenceUnavailable)?;
                    let verified = client
                        .put_verified(object, &bytes)
                        .await
                        .map_err(|_| Error::ReferenceUnavailable)?;
                    if verified.reference() != object {
                        return Err(Error::InvalidReconcileResult);
                    }
                }
                Err(_) => return Err(Error::ReferenceUnavailable),
            }
        }
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        let (current, verified) = load(&mut tx, org.as_str(), id)
            .await?
            .ok_or(Error::RuntimeAccessUnavailable)?;
        if current != m {
            return Err(Error::IdempotencyConflict);
        }
        let at = if let Some(at) = verified {
            at
        } else {
            let at = transactions::now(&mut tx).await?;
            sqlx::query("INSERT INTO execution_outputs (organization,execution_id,manifest_digest,verified_at_ms) VALUES ($1,$2,$3,$4)").bind(org.as_str()).bind(id).bind(m.digest()?).bind(at).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                org.as_str(),
                seq,
                "execution.output_verified",
                serde_json::json!({"execution_id":id,"manifest_digest":m.digest()?}),
            )
            .await?;
            at
        };
        let result = view(&m, Some(at))?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn candidate_execution_output(
        &self,
        token: &str,
        id: &str,
    ) -> Result<Option<ExecutionOutput>> {
        valid_id(id)?;
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeConnect).await?;
        own_execution(&mut tx, token, &identity, id).await?;
        let result = load(&mut tx, identity.organization().as_str(), id)
            .await?
            .map(|(m, v)| view(&m, v))
            .transpose()?;
        tx.commit().await?;
        Ok(result)
    }
    /// Trusted operator read; the report object is reverified, never a presigned URL.
    pub async fn read_candidate_execution_output(
        &self,
        org: &OrganizationId,
        id: &str,
        client: &Client,
    ) -> Result<Vec<u8>> {
        valid_id(id)?;
        let mut tx = self.pool.begin().await?;
        let (m, at) = load(&mut tx, org.as_str(), id)
            .await?
            .ok_or(Error::RuntimeAccessUnavailable)?;
        tx.commit().await?;
        if at.is_none() {
            return Err(Error::ReferenceUnavailable);
        }
        client
            .get(&m.objects[0])
            .await
            .map_err(|_| Error::ReferenceUnavailable)
    }
}
