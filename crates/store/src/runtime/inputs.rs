use super::*;
use agent_computer_core::identity::OrganizationId;
use agent_computer_storage::Manifest;

/// Called only after the start admission authorized the entire pinned graph,
/// including this Workspace's read and modify grants. Hashes are not credentials.
pub(crate) async fn select(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    workspace: &str,
    artifact: Option<&str>,
) -> Result<(i64, String, Option<String>)> {
    let revision = if let Some(artifact) = artifact {
        sqlx::query_scalar("SELECT v.revision FROM artifact_commits a JOIN workspace_input_versions v ON v.organization=a.organization AND v.workspace_id=a.workspace_id AND v.revision=a.input_revision AND v.artifact_commit_id=a.commit_id WHERE a.organization=$1 AND a.workspace_id=$2 AND a.commit_id=$3 AND a.state IN ('Committed','Conflict') AND a.object_ref IS NOT NULL")
            .bind(org).bind(workspace).bind(artifact).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?
    } else {
        current(tx, org, workspace).await?
    };
    let row=sqlx::query("SELECT digest,artifact_commit_id FROM workspace_input_versions WHERE organization=$1 AND workspace_id=$2 AND revision=$3")
        .bind(org).bind(workspace).bind(revision).fetch_one(&mut **tx).await?;
    Ok((
        revision,
        row.try_get("digest")?,
        row.try_get("artifact_commit_id")?,
    ))
}

pub(crate) async fn current(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    workspace: &str,
) -> Result<i64> {
    sqlx::query_scalar(
        "SELECT revision FROM workspace_input_heads WHERE organization=$1 AND workspace_id=$2",
    )
    .bind(org)
    .bind(workspace)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(Error::WorkspaceInputUnavailable)
}

/// Only for a newly created definition, or explicit trusted legacy initialization.
/// Never infer empty input at runtime from a missing head.
pub(crate) async fn initialize(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    workspace: &str,
) -> Result<()> {
    let manifest = Manifest::default();
    sqlx::query("INSERT INTO workspace_input_versions (organization,workspace_id,revision,manifest,digest,origin) VALUES ($1,$2,1,$3,$4,'initial_empty')")
        .bind(org).bind(workspace).bind(serde_json::to_value(&manifest).map_err(|_|Error::InvalidStoredData)?)
        .bind(manifest.digest().map_err(|_|Error::InvalidStoredData)?).execute(&mut **tx).await?;
    sqlx::query(
        "INSERT INTO workspace_input_heads (organization,workspace_id,revision) VALUES ($1,$2,1)",
    )
    .bind(org)
    .bind(workspace)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

impl Store {
    /// Trusted database administration, never a tenant-provided manifest or reset.
    /// The operator explicitly confirms that a legacy Workspace begins empty.
    pub async fn initialize_empty_workspace(
        &self,
        org: &OrganizationId,
        workspace: &str,
    ) -> Result<i64> {
        types::validate_target(RuntimeKind::Workspace, workspace, RuntimePermission::Manage)?;
        let mut tx = self.pool.begin().await?;
        let seq = Self::lock_stream(&mut tx, org.as_str()).await?;
        target(&mut tx, org.as_str(), RuntimeKind::Workspace, workspace).await?;
        match current(&mut tx, org.as_str(), workspace).await {
            Ok(revision) => {
                tx.commit().await?;
                return Ok(revision);
            }
            Err(Error::WorkspaceInputUnavailable) => {}
            Err(error) => return Err(error),
        }
        // Preserve legacy allocations: cancel and readmit them to bind an input.
        let dispatched: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM runtime_start_requests WHERE organization=$1 AND workspace_id=$2 AND state IN ('Preparing','Prepared'))")
            .bind(org.as_str()).bind(workspace).fetch_one(&mut *tx).await?;
        if dispatched {
            return Err(Error::RuntimeConflict);
        }
        initialize(&mut tx, org.as_str(), workspace).await?;
        transactions::emit(&mut tx, org.as_str(), seq, "workspace.initialized", serde_json::json!({"workspace_id":workspace,"input_revision":1,"origin":"initial_empty"})).await?;
        tx.commit().await?;
        Ok(1)
    }
}
