use super::*;

pub(super) async fn actor_access(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    app: &PgRow,
    caller: &Caller,
    permission: RuntimePermission,
    connected: bool,
) -> Result<()> {
    let computer: String = app.try_get("computer_id")?;
    let app_id: String = app.try_get("app_id")?;
    let profile: String = app.try_get("profile_id")?;
    let mut needs = vec![
        (
            RuntimeKind::Computer,
            computer.clone(),
            RuntimePermission::Read,
        ),
        (RuntimeKind::App, app_id.clone(), RuntimePermission::Read),
        (
            RuntimeKind::BrowserProfile,
            profile.clone(),
            RuntimePermission::Read,
        ),
    ];
    if permission != RuntimePermission::Read {
        needs.extend([
            (RuntimeKind::Computer, computer.clone(), permission),
            (RuntimeKind::App, app_id.clone(), permission),
        ]);
        if permission == RuntimePermission::Control {
            needs.extend([
                (
                    RuntimeKind::Computer,
                    computer.clone(),
                    RuntimePermission::Observe,
                ),
                (RuntimeKind::App, app_id.clone(), RuntimePermission::Observe),
                (
                    RuntimeKind::Computer,
                    computer.clone(),
                    RuntimePermission::AppUse,
                ),
                (RuntimeKind::App, app_id, RuntimePermission::AppUse),
            ]);
            needs.push((
                RuntimeKind::BrowserProfile,
                profile,
                RuntimePermission::AppUse,
            ));
        }
    }
    if connected {
        needs.push((
            RuntimeKind::Computer,
            computer.clone(),
            RuntimePermission::Connect,
        ));
    }
    for (kind, id, p) in needs {
        let identity = Store::authorize_bound_runtime_in(
            tx,
            org,
            &caller.principal,
            &caller.credential,
            p.scope(),
        )
        .await?;
        require_in(tx, &identity, &authority::requirement(kind, &id, p, 0)).await?;
    }
    if connected {
        let s=sqlx::query("SELECT requested,expires_at_ms FROM connection_sessions WHERE organization=$1 AND session_id=$2 AND principal=$3 AND credential_id=$4 AND computer_id=$5 AND state='Active'").bind(org).bind(&caller.session).bind(&caller.principal).bind(&caller.credential).bind(computer).fetch_optional(&mut **tx).await?.ok_or(Error::ConnectionInactive)?;
        let permissions: Vec<RuntimePermission> = serde_json::from_value(s.try_get("requested")?)
            .map_err(|_| Error::InvalidStoredData)?;
        if !permissions.contains(&RuntimePermission::Connect)
            || !permissions.contains(&RuntimePermission::Read)
            || (permission == RuntimePermission::Control
                && (!permissions.contains(&RuntimePermission::AppUse)
                    || !permissions.contains(&RuntimePermission::Observe)))
            || !permissions.contains(&permission)
            || transactions::now(tx).await? >= s.try_get("expires_at_ms")?
        {
            return Err(Error::ConnectionInactive);
        }
    }
    Ok(())
}
pub(super) async fn running(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    app: &PgRow,
) -> Result<()> {
    if app.try_get::<String, _>("state")? != "Running" {
        return Err(Error::RuntimeConflict);
    }
    authority::bound(tx, org, app).await
}
pub(super) async fn owns(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    who: &Caller,
    wanted: i64,
) -> Result<()> {
    let c = control(tx, org, id).await?.ok_or(Error::RuntimeConflict)?;
    if c.try_get::<String, _>("state")? != "Owned"
        || caller(&c)? != *who
        || c.try_get::<i64, _>("epoch")? != wanted
        || transactions::now(tx).await? >= c.try_get("expires_at_ms")?
    {
        return Err(Error::RuntimeConflict);
    }
    Ok(())
}
pub(super) async fn reconcile_control(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    app: &PgRow,
) -> Result<()> {
    let id: String = app.try_get("instance_id")?;
    if let Some(c) = control(tx, org, &id).await?
        && c.try_get::<String, _>("state")? == "Owned"
    {
        let access =
            actor_access(tx, org, app, &caller(&c)?, RuntimePermission::Control, true).await;
        let invalid = match access {
            Ok(()) => transactions::now(tx).await? >= c.try_get("expires_at_ms")?,
            Err(
                Error::Unauthenticated
                | Error::Forbidden
                | Error::RuntimeAccessUnavailable
                | Error::ConnectionInactive,
            ) => true,
            Err(e) => return Err(e),
        };
        if invalid {
            sqlx::query("UPDATE browser_controls SET state='Draining' WHERE organization=$1 AND instance_id=$2 AND state='Owned'").bind(org).bind(id).execute(&mut **tx).await?;
        }
    }
    Ok(())
}
pub(super) async fn command_access(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    app: &PgRow,
    who: &Caller,
    cmd: &BrowserCommand,
    wanted: i64,
) -> Result<()> {
    running(tx, org, app).await?;
    actor_access(tx, org, app, who, cmd.permission(), true).await?;
    let id: String = app.try_get("instance_id")?;
    if epoch(tx, org, &id).await? != wanted {
        return Err(Error::RuntimeConflict);
    }
    if matches!(cmd, BrowserCommand::Act { .. }) {
        owns(tx, org, &id, who, wanted).await?;
    }
    Ok(())
}
