use super::super::start::graph::Snapshot;
use super::*;
use crate::plans::DefinitionKind;

pub(super) fn requirement(
    kind: RuntimeKind,
    id: &str,
    permission: RuntimePermission,
    seconds: u32,
) -> RuntimeRequirement {
    RuntimeRequirement {
        kind,
        resource_id: id.into(),
        permission,
        runtime_seconds: (permission == RuntimePermission::Activate).then_some(seconds),
    }
}
pub(super) fn requirements(
    computer: &str,
    app: &str,
    profile: Option<&str>,
    seconds: u32,
) -> Vec<RuntimeRequirement> {
    let mut result = Vec::new();
    for (kind, id, permissions) in [
        (
            RuntimeKind::Computer,
            computer,
            &[
                RuntimePermission::Connect,
                RuntimePermission::Read,
                RuntimePermission::AppUse,
                RuntimePermission::Activate,
            ][..],
        ),
        (
            RuntimeKind::App,
            app,
            &[
                RuntimePermission::Read,
                RuntimePermission::AppUse,
                RuntimePermission::Activate,
            ][..],
        ),
    ] {
        result.extend(
            permissions
                .iter()
                .map(|p| requirement(kind, id, *p, seconds)),
        );
    }
    if let Some(profile) = profile {
        result.push(requirement(
            RuntimeKind::BrowserProfile,
            profile,
            RuntimePermission::AppUse,
            seconds,
        ));
    }
    result
}
pub(super) const CAPABILITIES: &[RuntimePermission] = &[
    RuntimePermission::Connect,
    RuntimePermission::Read,
    RuntimePermission::AppUse,
    RuntimePermission::Activate,
];

pub(super) async fn capture(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    computer: &str,
    input: &AdmitApp,
) -> Result<(String, Value, String, String)> {
    let start=sqlx::query("SELECT r.* FROM runtime_start_requests r JOIN runtime_controls c ON c.organization=r.organization AND c.computer_id=r.computer_id AND c.active_request=r.request_id AND c.generation=r.generation WHERE r.organization=$1 AND r.computer_id=$2 AND r.generation=$3 AND r.state='Prepared' AND EXISTS (SELECT 1 FROM candidate_preparations p WHERE p.organization=r.organization AND p.request_id=r.request_id AND p.receipt IS NOT NULL)")
        .bind(org).bind(computer).bind(input.computer_generation).fetch_optional(&mut **tx).await?.ok_or(Error::RuntimeConflict)?;
    let request: String = start.try_get("request_id")?;
    super::super::start::graph::validate_catalogs(tx, org, &request).await?;
    if input.runtime_seconds > start.try_get::<i32, _>("max_runtime_seconds")? as u32 {
        return Err(Error::RuntimeBudgetExceeded);
    }
    let snapshot: Snapshot =
        serde_json::from_value(start.try_get("snapshot")?).map_err(|_| Error::InvalidStoredData)?;
    let root = snapshot.resource(DefinitionKind::Computer, computer)?;
    if !root.spec["appRefs"]
        .as_array()
        .is_some_and(|v| v.contains(&json!(format!("id:{}", input.app_id))))
    {
        return Err(Error::RuntimeAccessUnavailable);
    }
    let app = snapshot.resource(DefinitionKind::App, &input.app_id)?;
    if app.reference.revision != input.expected_app_revision {
        return Err(Error::RevisionConflict);
    }
    if app.spec["driver"] != "chromium-playwright" {
        return Err(Error::ReferenceUnavailable);
    }
    let sandbox = reference(&app.spec["sandboxRef"])?;
    let sandbox = snapshot.resource(DefinitionKind::Sandbox, sandbox)?;
    if sandbox.spec["runtimeClass"] != "gvisor" {
        return Err(Error::ReferenceUnavailable);
    }
    // The start budget already reserves each unique Sandbox exactly once. The
    // instance's unique active slot prevents two Apps spending that reservation.
    let profile = reference(&app.spec["profileRef"])?.to_owned();
    Ok((
        request,
        json!({"app":app,"sandbox":sandbox}),
        sandbox.reference.resource_id.clone(),
        profile,
    ))
}
fn reference(value: &Value) -> Result<&str> {
    value
        .as_str()
        .and_then(|v| v.strip_prefix("id:"))
        .ok_or(Error::InvalidStoredData)
}

pub(super) async fn bound(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
) -> Result<()> {
    let principal: String = row.try_get("principal")?;
    let credential: String = row.try_get("credential_id")?;
    let computer: String = row.try_get("computer_id")?;
    let app: String = row.try_get("app_id")?;
    let profile: String = row.try_get("profile_id")?;
    let seconds = row.try_get::<i32, _>("runtime_seconds")? as u32;
    for need in requirements(&computer, &app, Some(&profile), seconds) {
        let identity = Store::authorize_bound_runtime_in(
            tx,
            org,
            &principal,
            &credential,
            need.permission.scope(),
        )
        .await?;
        require_in(tx, &identity, &need).await?;
    }
    let session=sqlx::query("SELECT requested,expires_at_ms FROM connection_sessions WHERE organization=$1 AND session_id=$2 AND principal=$3 AND credential_id=$4 AND computer_id=$5 AND state='Active'")
        .bind(org).bind(row.try_get::<String,_>("session_id")?).bind(&principal).bind(&credential).bind(&computer).fetch_optional(&mut **tx).await?.ok_or(Error::ConnectionInactive)?;
    let requested: Vec<RuntimePermission> = serde_json::from_value(session.try_get("requested")?)
        .map_err(|_| Error::InvalidStoredData)?;
    if !CAPABILITIES.iter().all(|p| requested.contains(p)) {
        return Err(Error::RuntimeAccessUnavailable);
    }
    let current=sqlx::query_scalar::<_,bool>("SELECT EXISTS (SELECT 1 FROM runtime_controls c JOIN runtime_start_requests r ON r.organization=c.organization AND r.request_id=c.active_request AND r.generation=c.generation WHERE c.organization=$1 AND c.computer_id=$2 AND r.request_id=$3 AND r.generation=$4 AND r.state='Prepared')")
        .bind(org).bind(&computer).bind(row.try_get::<String,_>("request_id")?).bind(row.try_get::<i64,_>("computer_generation")?).fetch_one(&mut **tx).await?;
    if !current {
        return Err(Error::RuntimeConflict);
    }
    super::super::start::graph::validate_catalogs(
        tx,
        org,
        &row.try_get::<String, _>("request_id")?,
    )
    .await?;
    let now = transactions::now(tx).await?;
    if now >= session.try_get::<i64, _>("expires_at_ms")?
        || now >= row.try_get::<i64, _>("expires_at_ms")?
    {
        return Err(Error::ConnectionInactive);
    }
    browser::check_deadline(tx, org, &row.try_get::<String, _>("instance_id")?).await?;
    Ok(())
}
pub(super) async fn reconcile(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    row: &PgRow,
    seq: i64,
) -> Result<()> {
    if !matches!(
        row.try_get::<String, _>("state")?.as_str(),
        "Queued" | "Dispatching" | "Running"
    ) {
        return Ok(());
    }
    match bound(tx, org, row).await {
        Ok(()) => Ok(()),
        Err(
            Error::Unauthenticated
            | Error::Forbidden
            | Error::RuntimeAccessUnavailable
            | Error::RuntimeBudgetExceeded
            | Error::ConnectionInactive
            | Error::RuntimeConflict
            | Error::ReferenceUnavailable,
        ) => {
            let id: String = row.try_get("instance_id")?;
            sqlx::query("UPDATE app_instances SET state=CASE WHEN state='Queued' THEN 'Blocked' ELSE 'Draining' END,revision=revision+1 WHERE organization=$1 AND instance_id=$2 AND state IN ('Queued','Dispatching','Running')").bind(org).bind(&id).execute(&mut **tx).await?;
            transactions::emit(
                tx,
                org,
                seq,
                if row.try_get::<String, _>("state")? == "Queued" {
                    "app.admission_blocked"
                } else {
                    "app.browser_authority_invalidated"
                },
                json!({"instance_id":id,"dispatch_started":row.try_get::<String, _>("state")? != "Queued"}),
            )
            .await?;
            Ok(())
        }
        Err(error) => Err(error),
    }
}
