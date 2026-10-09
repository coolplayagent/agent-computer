use super::*;
use agent_computer_core::identity::OrganizationId;
use agent_computer_storage::Manifest;

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
