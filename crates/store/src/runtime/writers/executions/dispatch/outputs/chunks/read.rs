use super::*;

#[derive(Debug, Serialize)]
pub struct ExecutionChunkPage {
    pub execution_id: String,
    pub enabled: bool,
    pub execution_state: ExecutionState,
    pub after_sequence: u32,
    pub next_sequence: u32,
    pub available_sequence: u32,
    pub final_sequence: Option<u32>,
    /// A verified report remains an observation, not an accepted completion.
    pub final_report_verified: bool,
    pub chunks: Vec<ExecutionOutputChunk>,
}
pub struct ExecutionChunkDownload {
    pub bytes: Vec<u8>,
    pub chunk: ExecutionOutputChunk,
}
impl Store {
    /// Stable cursor over a contiguous verified prefix. Neither polling nor a
    /// terminal-looking output frame changes execution or writer authority.
    pub async fn candidate_execution_chunks(
        &self,
        token: &str,
        id: &str,
        after: u32,
        limit: u32,
    ) -> Result<ExecutionChunkPage> {
        valid_id(id)?;
        if after > MAX_CHUNKS || !(1..=32).contains(&limit) {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let r = super::super::download::authorize_read(&mut tx, token, &identity, id).await?;
        let execution = super::super::super::super::view(&r)?;
        let org = identity.organization().as_str();
        let records=sqlx::query("SELECT i.sequence,i.manifest,i.manifest_digest,c.verified_at_ms FROM execution_output_chunk_intents i JOIN execution_output_chunks c USING(organization,execution_id,sequence) WHERE i.organization=$1 AND i.execution_id=$2 AND i.sequence>$3 ORDER BY i.sequence LIMIT $4")
            .bind(org).bind(id).bind(after as i32).bind(limit as i64).fetch_all(&mut *tx).await?;
        let mut chunks = Vec::new();
        for record in records {
            let (m, at) = decode(record, org, id)?;
            chunks.push(view(&m, at));
        }
        let available:i32=sqlx::query_scalar("SELECT COALESCE(max(sequence),0) FROM execution_output_chunks WHERE organization=$1 AND execution_id=$2").bind(org).bind(id).fetch_one(&mut *tx).await?;
        let final_output = super::super::load(&mut tx, org, id).await?;
        let final_sequence = final_output
            .as_ref()
            .and_then(|(m, _)| m.stream.as_ref().map(|s| s.sequence));
        let final_report_verified = final_output.as_ref().is_some_and(|(_, at)| at.is_some());
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(ExecutionChunkPage {
            execution_id: id.into(),
            enabled: execution.stream_output,
            execution_state: execution.state,
            after_sequence: after,
            next_sequence: chunks.last().map_or(after, |c| c.sequence),
            available_sequence: available as u32,
            final_sequence,
            final_report_verified,
            chunks,
        })
    }
    async fn readable_execution_chunk(
        &self,
        token: &str,
        id: &str,
        sequence: u32,
    ) -> Result<(ChunkManifest, i64)> {
        valid_id(id)?;
        if !(1..=MAX_CHUNKS).contains(&sequence) {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        super::super::download::authorize_read(&mut tx, token, &identity, id).await?;
        let (m, at) = load(&mut tx, identity.organization().as_str(), id, sequence)
            .await?
            .ok_or(Error::ExecutionOutputUnavailable)?;
        let at = at.ok_or(Error::ExecutionOutputUnavailable)?;
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok((m, at))
    }
    /// Authorize before and after every verified object read, including errors.
    /// A later execution transition does not change this immutable chunk.
    pub async fn download_candidate_execution_chunk(
        &self,
        token: &str,
        id: &str,
        sequence: u32,
        client: &Client,
    ) -> Result<ExecutionChunkDownload> {
        let admitted = self.readable_execution_chunk(token, id, sequence).await?;
        let bytes = client.get(&admitted.0.object).await;
        let current = self.readable_execution_chunk(token, id, sequence).await?;
        if current != admitted {
            return Err(Error::RuntimeConflict);
        }
        let bytes = current
            .0
            .verify_bytes(bytes.map_err(|_| Error::ExecutionOutputUnavailable)?)?;
        Ok(ExecutionChunkDownload {
            bytes,
            chunk: view(&current.0, Some(current.1)),
        })
    }
}
