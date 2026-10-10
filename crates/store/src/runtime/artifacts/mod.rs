//! Seal bounded-file Candidates and publish immutable Workspace versions by CAS.
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey, OrganizationId};
use agent_computer_objects::artifact::{Bundle, CapturedArtifact, VerifiedArtifact};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
mod admission;
mod queue;
mod types;
mod worker;
pub use types::*;

fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(|_| Error::InvalidStoredData)
}
async fn row(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    sqlx::query("SELECT a.*,r.computer_id,r.candidate_id,r.generation,r.snapshot_digest,s.receipt AS stop_receipt FROM artifact_commits a JOIN runtime_start_requests r USING(organization,request_id) LEFT JOIN runtime_stops s USING(organization,request_id) WHERE a.organization=$1 AND a.commit_id=$2")
        .bind(org).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)
}
fn view(row: &PgRow) -> Result<ArtifactCommit> {
    let input: CommitArtifact = decode(row.try_get("input")?)?;
    let capture: Option<Bundle> = row
        .try_get::<Option<serde_json::Value>, _>("capture")?
        .map(decode)
        .transpose()?;
    Ok(ArtifactCommit {
        commit_id: row.try_get("commit_id")?,
        workspace_id: row.try_get("workspace_id")?,
        computer_id: row.try_get("computer_id")?,
        candidate_id: row.try_get("candidate_id")?,
        generation: row.try_get("generation")?,
        creator: row.try_get("principal")?,
        state: decode(serde_json::Value::String(row.try_get("state")?))?,
        base_revision: input.base_revision,
        base_manifest: input.base_manifest,
        publish_current: input.publish_current,
        input_revision: row.try_get("input_revision")?,
        manifest_digest: capture
            .map(|v| v.manifest.digest().map_err(|_| Error::InvalidStoredData))
            .transpose()?,
        published_at_ms: row.try_get("published_at_ms")?,
        stop_after_commit: row.try_get("stop_after_commit")?,
        stop_receipt: if row.try_get("stop_after_commit")? {
            row.try_get::<Option<serde_json::Value>, _>("stop_receipt")?
                .map(decode)
                .transpose()?
        } else {
            None
        },
    })
}
fn requirements(workspace: &str, computer: &str, stop: bool) -> Vec<RuntimeRequirement> {
    let mut needs: Vec<_> = [
        (RuntimeKind::Workspace, workspace, RuntimePermission::Read),
        (RuntimeKind::Workspace, workspace, RuntimePermission::Modify),
        (
            RuntimeKind::Workspace,
            workspace,
            RuntimePermission::Publish,
        ),
        (RuntimeKind::Computer, computer, RuntimePermission::Read),
        (RuntimeKind::Computer, computer, RuntimePermission::Modify),
    ]
    .into_iter()
    .map(|(kind, id, permission)| RuntimeRequirement {
        kind,
        resource_id: id.into(),
        permission,
        runtime_seconds: None,
    })
    .collect();
    if stop {
        needs.push(RuntimeRequirement {
            kind: RuntimeKind::Computer,
            resource_id: computer.into(),
            permission: RuntimePermission::Manage,
            runtime_seconds: None,
        });
    }
    needs
}
impl Store {
    pub async fn workspace_artifact(&self, token: &str, id: &str) -> Result<ArtifactCommit> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let record = row(&mut tx, identity.organization().as_str(), id).await?;
        authorize_in(
            &mut tx,
            token,
            &[RuntimeRequirement {
                kind: RuntimeKind::Workspace,
                resource_id: record.try_get("workspace_id")?,
                permission: RuntimePermission::Read,
                runtime_seconds: None,
            }],
        )
        .await?;
        let result = view(&record)?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn artifact_manifest(
        &self,
        token: &str,
        id: &str,
    ) -> Result<Option<agent_computer_storage::Manifest>> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let record = row(&mut tx, identity.organization().as_str(), id).await?;
        authorize_in(
            &mut tx,
            token,
            &[RuntimeRequirement {
                kind: RuntimeKind::Workspace,
                resource_id: record.try_get("workspace_id")?,
                permission: RuntimePermission::Read,
                runtime_seconds: None,
            }],
        )
        .await?;
        let result = if record.try_get::<String, _>("state")? == "Capturing" {
            None
        } else {
            Some(decode::<Bundle>(record.try_get("capture")?)?.manifest)
        };
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(result)
    }
}
