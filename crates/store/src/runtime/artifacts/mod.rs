//! Seal bounded-file Candidates and publish immutable Workspace versions by CAS.
use super::*;
use crate::plans::types::{digest, random_id};
use agent_computer_core::identity::{ComputerId, IdempotencyKey, OrganizationId};
use agent_computer_objects::artifact::{Bundle, CapturedArtifact, VerifiedArtifact};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
mod types;
mod worker;
pub use types::*;

fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(|_| Error::InvalidStoredData)
}
async fn row(tx: &mut Transaction<'_, Postgres>, org: &str, id: &str) -> Result<PgRow> {
    sqlx::query("SELECT a.*,r.computer_id,r.candidate_id,r.generation,r.snapshot_digest FROM artifact_commits a JOIN runtime_start_requests r USING(organization,request_id) WHERE a.organization=$1 AND a.commit_id=$2")
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
    })
}
fn requirements(workspace: &str, computer: &str) -> Vec<RuntimeRequirement> {
    [
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
    .collect()
}
impl Store {
    pub async fn commit_workspace_artifact(
        &self,
        token: &str,
        key: &IdempotencyKey,
        workspace: &str,
        input: &CommitArtifact,
    ) -> Result<ArtifactCommit> {
        if ComputerId::new(&input.request_id).is_err()
            || ComputerId::new(workspace).is_err()
            || input.expected_revision < 1
            || input.base_revision < 1
            || !agent_computer_objects::digest(&input.base_manifest)
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimePublish).await?;
        let org = identity.organization().as_str();
        // Authorize Workspace before resolving a potentially private start request.
        authorize_in(
            &mut tx,
            token,
            &[RuntimeRequirement {
                kind: RuntimeKind::Workspace,
                resource_id: workspace.into(),
                permission: RuntimePermission::Publish,
                runtime_seconds: None,
            }],
        )
        .await?;
        let source=sqlx::query("SELECT r.*,c.revision AS control_revision,c.active_request,p.receipt AS prepared_receipt,i.revision AS input_revision,v.digest AS input_digest FROM runtime_start_requests r JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id JOIN candidate_preparations p ON p.organization=r.organization AND p.request_id=r.request_id JOIN runtime_start_inputs i ON i.organization=r.organization AND i.request_id=r.request_id JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision WHERE r.organization=$1 AND r.request_id=$2 AND r.workspace_id=$3")
            .bind(org).bind(&input.request_id).bind(workspace).fetch_optional(&mut *tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
        let computer: String = source.try_get("computer_id")?;
        let permissions = requirements(workspace, &computer);
        authorize_in(&mut tx, token, &permissions).await?;
        let hash = digest("agent-computer/artifact-commit-v1", &(workspace, input))?;
        let op = "runtime.artifact-commit.v1";
        if let Some(id) = transactions::retry::<String>(&mut tx, &identity, op, key, &hash).await? {
            let existing = row(&mut tx, org, &id).await?;
            if existing.try_get::<String, _>("state")? == "Capturing"
                && existing.try_get::<String, _>("credential_id")? != crate::auth::token_id(token)?
            {
                sqlx::query("UPDATE artifact_commits SET credential_id=$3,lease_epoch=lease_epoch+1,lease_owner=NULL,lease_until_ms=NULL WHERE organization=$1 AND commit_id=$2").bind(org).bind(&id).bind(crate::auth::token_id(token)?).execute(&mut *tx).await?;
                transactions::emit(
                    &mut tx,
                    org,
                    seq,
                    "artifact.authorization_refreshed",
                    serde_json::json!({"commit_id":id}),
                )
                .await?;
            }
            let result = view(&existing)?;
            authorize_in(&mut tx, token, &permissions).await?;
            tx.commit().await?;
            return Ok(result);
        }
        if source.try_get::<String, _>("state")? != "Prepared"
            || source
                .try_get::<Option<String>, _>("active_request")?
                .as_deref()
                != Some(&input.request_id)
            || source.try_get::<i64, _>("control_revision")? != input.expected_revision
            || source.try_get::<i64, _>("input_revision")? != input.base_revision
            || source.try_get::<String, _>("input_digest")? != input.base_manifest
            || source
                .try_get::<Option<serde_json::Value>, _>("prepared_receipt")?
                .is_none()
        {
            return Err(Error::RuntimeConflict);
        }
        let clean: bool = sqlx::query_scalar("SELECT artifact_candidate_drained($1,$2)")
            .bind(org)
            .bind(&input.request_id)
            .fetch_one(&mut *tx)
            .await?;
        if !clean {
            return Err(Error::WriterLeaseBusy);
        }
        super::start::graph::validate_catalogs(&mut tx, org, &input.request_id).await?;
        let id = random_id("artifact")?;
        sqlx::query("INSERT INTO artifact_commits (organization,commit_id,request_id,workspace_id,principal,credential_id,input) VALUES ($1,$2,$3,$4,$5,$6,$7)").bind(org).bind(&id).bind(&input.request_id).bind(workspace).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?).bind(serde_json::to_value(input).map_err(|_|Error::InvalidRuntimeRequest)?).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_start_requests SET state='Sealing' WHERE organization=$1 AND request_id=$2").bind(org).bind(&input.request_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_controls SET revision=revision+1 WHERE organization=$1 AND computer_id=$2").bind(org).bind(&computer).execute(&mut *tx).await?;
        transactions::emit(&mut tx,org,seq,"artifact.sealing",serde_json::json!({"commit_id":id,"workspace_id":workspace,"request_id":input.request_id,"base_revision":input.base_revision,"publish_current":input.publish_current})).await?;
        transactions::save_receipt(&mut tx, &identity, op, key, &hash, &id).await?;
        let result = view(&row(&mut tx, org, &id).await?)?;
        authorize_in(&mut tx, token, &permissions).await?;
        tx.commit().await?;
        Ok(result)
    }
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
