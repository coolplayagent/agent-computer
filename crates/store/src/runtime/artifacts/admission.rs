use super::*;

struct Admission<'a> {
    token: &'a str,
    key: &'a IdempotencyKey,
    workspace: &'a str,
    input: &'a CommitArtifact,
    stop: bool,
    cancel_running: bool,
}

pub(super) async fn available(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    request: &str,
    principal: &str,
) -> Result<()> {
    check_stop(tx, org, request, principal, false).await
}

pub(super) async fn eligible(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    request: &str,
    principal: &str,
) -> Result<()> {
    check_stop(tx, org, request, principal, true).await
}

async fn check_stop(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    request: &str,
    principal: &str,
    pending_drain: bool,
) -> Result<()> {
    let query = if pending_drain {
        "SELECT checkpoint_stop_eligible($1,$2,$3)"
    } else {
        "SELECT checkpoint_stop_available($1,$2,$3)"
    };
    let allowed: bool = sqlx::query_scalar(query)
        .bind(org)
        .bind(request)
        .bind(principal)
        .fetch_one(&mut **tx)
        .await?;
    if allowed {
        Ok(())
    } else {
        let active: bool = sqlx::query_scalar("SELECT checkpoint_stop_active_use($1,$2,$3)")
            .bind(org)
            .bind(request)
            .bind(principal)
            .fetch_one(&mut **tx)
            .await?;
        Err(if active {
            Error::RuntimeActiveUse
        } else {
            Error::RuntimeStopBlocked
        })
    }
}

impl Store {
    pub async fn commit_workspace_artifact(
        &self,
        token: &str,
        key: &IdempotencyKey,
        workspace: &str,
        input: &CommitArtifact,
    ) -> Result<ArtifactCommit> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimePublish).await?;
        let result = admit(
            &mut tx,
            &identity,
            seq,
            Admission {
                token,
                key,
                workspace,
                input,
                stop: false,
                cancel_running: false,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Admit a normal file-checkpoint stop. Publication and the stop receipt
    /// finish together; accepted work alone never reports a stopped Computer.
    pub async fn checkpoint_stop_computer(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        request: &CheckpointStop,
    ) -> Result<ArtifactCommit> {
        if ComputerId::new(computer).is_err()
            || ComputerId::new(&request.request_id).is_err()
            || request.expected_revision < 1
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeManage).await?;
        authorize_in(
            &mut tx,
            token,
            &[RuntimeRequirement {
                kind: RuntimeKind::Computer,
                resource_id: computer.into(),
                permission: RuntimePermission::Manage,
                runtime_seconds: None,
            }],
        )
        .await?;
        let source = sqlx::query("SELECT r.workspace_id,i.revision,v.digest FROM runtime_start_requests r JOIN runtime_start_inputs i USING(organization,request_id) JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision WHERE r.organization=$1 AND r.request_id=$2 AND r.computer_id=$3")
            .bind(identity.organization().as_str()).bind(&request.request_id).bind(computer).fetch_optional(&mut *tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
        let workspace: String = source.try_get("workspace_id")?;
        let input = CommitArtifact {
            request_id: request.request_id.clone(),
            expected_revision: request.expected_revision,
            base_revision: source.try_get("revision")?,
            base_manifest: source.try_get("digest")?,
            publish_current: request.publish_current,
        };
        let result = admit(
            &mut tx,
            &identity,
            seq,
            Admission {
                token,
                key,
                workspace: &workspace,
                input: &input,
                stop: true,
                cancel_running: request.cancel_running,
            },
        )
        .await?;
        tx.commit().await?;
        Ok(result)
    }
}

async fn admit(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    seq: i64,
    admission: Admission<'_>,
) -> Result<ArtifactCommit> {
    let Admission {
        token,
        key,
        workspace,
        input,
        stop,
        cancel_running,
    } = admission;
    if ComputerId::new(&input.request_id).is_err()
        || ComputerId::new(workspace).is_err()
        || input.expected_revision < 1
        || input.base_revision < 1
        || !agent_computer_objects::digest(&input.base_manifest)
    {
        return Err(Error::InvalidRuntimeRequest);
    }
    let org = identity.organization().as_str();
    // Authorize Workspace before resolving a potentially private start request.
    authorize_in(
        tx,
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
            .bind(org).bind(&input.request_id).bind(workspace).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeAccessUnavailable)?;
    let computer: String = source.try_get("computer_id")?;
    let permissions = requirements(workspace, &computer, stop);
    authorize_in(tx, token, &permissions).await?;
    let (domain, op) = if stop {
        (
            "agent-computer/checkpoint-stop-v1",
            "runtime.checkpoint-stop.v1",
        )
    } else {
        (
            "agent-computer/artifact-commit-v1",
            "runtime.artifact-commit.v1",
        )
    };
    let hash = if cancel_running {
        digest(domain, &(workspace, input, "cancel_running"))?
    } else {
        // Preserve the previously admitted operation's canonical hash.
        digest(domain, &(workspace, input))?
    };
    if let Some(id) = transactions::retry::<String>(tx, identity, op, key, &hash).await? {
        let existing = row(tx, org, &id).await?;
        if matches!(
            existing.try_get::<String, _>("state")?.as_str(),
            "Draining" | "Capturing"
        ) && existing.try_get::<String, _>("credential_id")? != crate::auth::token_id(token)?
        {
            sqlx::query("UPDATE artifact_commits SET credential_id=$3,lease_epoch=lease_epoch+1,lease_owner=NULL,lease_until_ms=NULL WHERE organization=$1 AND commit_id=$2").bind(org).bind(&id).bind(crate::auth::token_id(token)?).execute(&mut **tx).await?;
            transactions::emit(
                tx,
                org,
                seq,
                "artifact.authorization_refreshed",
                serde_json::json!({"commit_id":id}),
            )
            .await?;
        }
        let result = view(&existing)?;
        authorize_in(tx, token, &permissions).await?;
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
        .fetch_one(&mut **tx)
        .await?;
    if !clean && !cancel_running {
        return Err(Error::WriterLeaseBusy);
    }
    super::start::graph::validate_catalogs(tx, org, &input.request_id).await?;
    if stop {
        check_stop(
            tx,
            org,
            &input.request_id,
            identity.principal().as_str(),
            cancel_running,
        )
        .await?;
    }
    let id = random_id("artifact")?;
    sqlx::query("INSERT INTO artifact_commits (organization,commit_id,request_id,workspace_id,principal,credential_id,input,stop_after_commit,cancel_running,state) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(org).bind(&id).bind(&input.request_id).bind(workspace).bind(identity.principal().as_str()).bind(crate::auth::token_id(token)?).bind(serde_json::to_value(input).map_err(|_|Error::InvalidRuntimeRequest)?).bind(stop).bind(cancel_running).bind(if cancel_running { "Draining" } else { "Capturing" }).execute(&mut **tx).await?;
    sqlx::query(
        "UPDATE runtime_start_requests SET state=$3 WHERE organization=$1 AND request_id=$2",
    )
    .bind(org)
    .bind(&input.request_id)
    .bind(if cancel_running {
        "Draining"
    } else {
        "Sealing"
    })
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE runtime_controls SET revision=revision+1 WHERE organization=$1 AND computer_id=$2",
    )
    .bind(org)
    .bind(&computer)
    .execute(&mut **tx)
    .await?;
    let seq = transactions::emit(tx,org,seq,if cancel_running { "computer.drain_requested" } else { "artifact.sealing" },serde_json::json!({"commit_id":id,"workspace_id":workspace,"request_id":input.request_id,"base_revision":input.base_revision,"publish_current":input.publish_current,"stop_after_commit":stop,"cancel_running":cancel_running})).await?;
    if cancel_running {
        super::super::writers::request_checkpoint_drain(tx, org, &input.request_id, seq).await?;
    }
    transactions::save_receipt(tx, identity, op, key, &hash, &id).await?;
    let result = view(&row(tx, org, &id).await?)?;
    authorize_in(tx, token, &permissions).await?;
    Ok(result)
}
