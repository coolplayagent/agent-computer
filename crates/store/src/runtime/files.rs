//! Read admission is independent of modification ownership. Recheck it after IO
//! before disclosing bytes; never retain a database lock while waiting on FUSE.
use super::*;
use agent_computer_core::identity::ComputerId;
use agent_computer_storage::Prepared;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadFileRequest {
    pub connection_session_id: String,
    pub generation: i64,
    pub candidate_id: String,
    pub path: String,
}

#[derive(Debug, Eq, PartialEq)]
pub struct CandidateRead {
    pub prepared: Prepared,
    pub target: preparation::PreparationTarget,
}
impl Store {
    /// Both pre-IO admission and post-IO disclosure must use this check. The
    /// caller compares both returned bindings so stale bytes cannot cross a
    /// generation/preparation change. This is not a persistent read capability.
    pub async fn candidate_file_read(
        &self,
        token: &str,
        workspace: &str,
        input: &ReadFileRequest,
    ) -> Result<CandidateRead> {
        for id in [workspace, &input.connection_session_id, &input.candidate_id] {
            ComputerId::new(id).map_err(|_| Error::InvalidRuntimeRequest)?;
        }
        if input.generation < 1
            || agent_computer_storage::files::validate_file_path(&input.path).is_err()
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let session =
            connections::final_view(&mut tx, token, &identity, &input.connection_session_id)
                .await?;
        if session.state != connections::ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        if !session.capabilities.contains(&RuntimePermission::Read) {
            return Err(Error::RuntimeAccessUnavailable);
        }
        authorize_in(
            &mut tx,
            token,
            &[
                RuntimeRequirement {
                    kind: RuntimeKind::Computer,
                    resource_id: session.computer_id.clone(),
                    permission: RuntimePermission::Read,
                    runtime_seconds: None,
                },
                RuntimeRequirement {
                    kind: RuntimeKind::Workspace,
                    resource_id: workspace.into(),
                    permission: RuntimePermission::Read,
                    runtime_seconds: None,
                },
            ],
        )
        .await?;
        let row=sqlx::query("SELECT r.request_id,p.receipt,p.binding FROM runtime_controls c JOIN runtime_start_requests r ON r.organization=c.organization AND r.computer_id=c.computer_id AND r.request_id=c.active_request AND r.generation=c.generation JOIN candidate_preparations p ON p.organization=r.organization AND p.request_id=r.request_id WHERE c.organization=$1 AND c.computer_id=$2 AND r.workspace_id=$3 AND r.generation=$4 AND r.candidate_id=$5 AND r.state='Prepared' AND p.receipt IS NOT NULL")
            .bind(identity.organization().as_str()).bind(&session.computer_id).bind(workspace).bind(input.generation).bind(&input.candidate_id).fetch_optional(&mut *tx).await?.ok_or(Error::RuntimeConflict)?;
        start::graph::validate_catalogs(
            &mut tx,
            identity.organization().as_str(),
            &row.try_get::<String, _>("request_id")?,
        )
        .await?;
        let result = CandidateRead {
            prepared: serde_json::from_value(row.try_get("receipt")?)
                .map_err(|_| Error::InvalidStoredData)?,
            target: serde_json::from_value(row.try_get("binding")?)
                .map_err(|_| Error::InvalidStoredData)?,
        };
        let session =
            connections::final_view(&mut tx, token, &identity, &input.connection_session_id)
                .await?;
        if session.state != connections::ConnectionState::Active {
            return Err(Error::ConnectionInactive);
        }
        Self::authorize_service_in(&mut tx, token, ServiceScope::RuntimeRead).await?;
        tx.commit().await?;
        Ok(result)
    }
}
