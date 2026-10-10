//! Historical output reads never acquire a writer or change execution state.
use super::*;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    Stdout,
    Stderr,
}

/// Verified bytes and public metadata only; no object-store address or secret.
pub struct ExecutionOutputDownload {
    pub bytes: Vec<u8>,
    pub output: ExecutionOutput,
}

impl Store {
    async fn readable_execution_output(&self, token: &str, id: &str) -> Result<(Manifest, i64)> {
        valid_id(id)?;
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let record = own_execution(&mut tx, token, &identity, id).await?;
        let execution = super::super::super::view(&record)?;
        // The immutable epoch keeps this read bound to the original Workspace
        // after the writer slot advances. A current Candidate is not required.
        let workspace: String = sqlx::query_scalar("SELECT r.workspace_id FROM candidate_writer_epochs e JOIN candidate_writer_leases l USING(organization,lease_id) JOIN runtime_start_requests r USING(organization,request_id) WHERE e.organization=$1 AND e.lease_id=$2 AND e.epoch=$3 AND r.computer_id=$4 AND r.candidate_id=$5 AND r.generation=$6")
            .bind(identity.organization().as_str()).bind(&execution.lease_id).bind(execution.epoch)
            .bind(&execution.computer_id).bind(&execution.candidate_id).bind(execution.generation)
            .fetch_optional(&mut *tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
        authorize_in(
            &mut tx,
            token,
            &[
                RuntimeRequirement {
                    kind: RuntimeKind::Computer,
                    resource_id: execution.computer_id.clone(),
                    permission: RuntimePermission::Connect,
                    runtime_seconds: None,
                },
                RuntimeRequirement {
                    kind: RuntimeKind::Computer,
                    resource_id: execution.computer_id,
                    permission: RuntimePermission::Read,
                    runtime_seconds: None,
                },
                RuntimeRequirement {
                    kind: RuntimeKind::Workspace,
                    resource_id: workspace,
                    permission: RuntimePermission::Read,
                    runtime_seconds: None,
                },
            ],
        )
        .await?;
        let (manifest, verified) = load(&mut tx, identity.organization().as_str(), id)
            .await?
            .ok_or(Error::ExecutionOutputUnavailable)?;
        let verified = verified.ok_or(Error::ExecutionOutputUnavailable)?;
        let input: SubmitExecution = serde_json::from_value(record.try_get("input")?)
            .map_err(|_| Error::InvalidStoredData)?;
        input
            .command
            .validate()
            .map_err(|_| Error::InvalidStoredData)?;
        if manifest.objects[1..3]
            .iter()
            .any(|object| object.size > input.command.output_limit_bytes as u64)
        {
            return Err(Error::InvalidStoredData);
        }
        // Closing/expiring the original connection ends live input, not access
        // to retained output. The same credential and current resource grants
        // are required, including immediately before disclosing fetched bytes.
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok((manifest, verified))
    }

    /// Authenticated, bounded output read. No database lock spans network IO.
    /// Recheck authorization even when GET fails, so revocation cannot expose
    /// object existence/integrity. Every GET verifies the fixed size and SHA-256.
    pub async fn download_candidate_execution_output(
        &self,
        token: &str,
        id: &str,
        stream: OutputStream,
        client: &Client,
    ) -> Result<ExecutionOutputDownload> {
        let admitted = self.readable_execution_output(token, id).await?;
        let index = match stream {
            OutputStream::Stdout => 1,
            OutputStream::Stderr => 2,
        };
        let bytes = client.get(&admitted.0.objects[index]).await;
        let current = self.readable_execution_output(token, id).await?;
        if current != admitted {
            return Err(Error::RuntimeConflict);
        }
        Ok(ExecutionOutputDownload {
            bytes: bytes.map_err(|_| Error::ExecutionOutputUnavailable)?,
            output: view(&current.0, Some(current.1))?,
        })
    }
}
