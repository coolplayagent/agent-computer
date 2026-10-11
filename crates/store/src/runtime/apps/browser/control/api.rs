use super::*;
impl Store {
    pub async fn acquire_browser_control(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &AcquireControl,
    ) -> Result<BrowserControl> {
        if !(1..=30_000).contains(&input.lifetime_ms)
            || input.app_generation < 1
            || input.expected_epoch < 0
        {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeControl).await?;
        let org = identity.organization().as_str();
        let app = row(&mut tx, org, id).await?;
        let who = authenticated(&identity, token, &input.connection_session_id)?;
        actor_access(&mut tx, org, &app, &who, RuntimePermission::Control, true).await?;
        let hash = digest("agent-computer/browser-control-v1", &(id, &who, input))?;
        let op = "runtime.browser-control.v1";
        let retry = transactions::retry::<i64>(&mut tx, &identity, op, key, &hash).await?;
        reconcile_control(&mut tx, org, &app).await?;
        if retry.is_none() {
            running(&mut tx, org, &app).await?;
            if app.try_get::<i64, _>("app_generation")? != input.app_generation {
                return Err(Error::RuntimeConflict);
            }
            let old = control(&mut tx, org, id).await?;
            let previous = old
                .as_ref()
                .map(|r| r.try_get::<i64, _>("epoch"))
                .transpose()?
                .unwrap_or(0);
            if previous != input.expected_epoch
                || old
                    .as_ref()
                    .is_some_and(|r| r.get::<String, _>("state") != "Idle")
            {
                tx.commit().await?;
                return Err(Error::RuntimeConflict);
            }
            let next = previous
                .checked_add(1)
                .filter(|n| *n < (1_i64 << 53))
                .ok_or(Error::CounterExhausted)?;
            let now = transactions::now(&mut tx).await?;
            let arm = require(&mut tx, org, id, "arm").await?;
            let expires = (now + i64::from(input.lifetime_ms))
                .min(app.try_get("expires_at_ms")?)
                .min(
                    arm.payload["expires_at_ms"]
                        .as_i64()
                        .ok_or(Error::InvalidStoredData)?,
                );
            if expires <= now {
                return Err(Error::RuntimeConflict);
            }
            // INSERT's initial-epoch trigger also runs before an upsert conflict.
            // Use a real update for an already drained epoch under the stream lock.
            sqlx::query(if old.is_none() {
                "INSERT INTO browser_controls (organization,instance_id,epoch,state,principal,credential_id,session_id,expires_at_ms) VALUES($1,$2,$3,'Owned',$4,$5,$6,$7)"
            } else {
                "UPDATE browser_controls SET epoch=$3,state='Owned',principal=$4,credential_id=$5,session_id=$6,expires_at_ms=$7,drain_nonce=NULL WHERE organization=$1 AND instance_id=$2 AND state='Idle' AND epoch=$3-1"
            })
                .bind(org).bind(id).bind(next).bind(&who.principal).bind(&who.credential).bind(&who.session).bind(expires).execute(&mut *tx).await?;
            transactions::save_receipt(&mut tx, &identity, op, key, &hash, &next).await?;
            transactions::emit(
                &mut tx,
                org,
                seq,
                "browser.control_acquired",
                json!({"instance_id":id,"epoch":next,"connection_session_id":who.session}),
            )
            .await?;
        }
        let c = control(&mut tx, org, id)
            .await?
            .ok_or(Error::RuntimeConflict)?;
        if retry.is_some_and(|e| e != c.get::<i64, _>("epoch")) {
            return Err(Error::RuntimeConflict);
        }
        let result = control_view(&c)?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn release_browser_control(
        &self,
        token: &str,
        id: &str,
        input: &ReleaseControl,
    ) -> Result<BrowserControl> {
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeControl).await?;
        let org = identity.organization().as_str();
        let app = row(&mut tx, org, id).await?;
        let who = authenticated(&identity, token, &input.connection_session_id)?;
        actor_access(&mut tx, org, &app, &who, RuntimePermission::Control, false).await?;
        let c = control(&mut tx, org, id)
            .await?
            .ok_or(Error::RuntimeConflict)?;
        if caller(&c)? != who || c.try_get::<i64, _>("epoch")? != input.control_epoch {
            return Err(Error::RuntimeConflict);
        }
        if c.try_get::<String, _>("state")? == "Owned" {
            sqlx::query("UPDATE browser_controls SET state='Draining' WHERE organization=$1 AND instance_id=$2").bind(org).bind(id).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                org,
                seq,
                "browser.control_drain_requested",
                json!({"instance_id":id,"epoch":input.control_epoch}),
            )
            .await?;
        }
        let result = control_view(
            &control(&mut tx, org, id)
                .await?
                .ok_or(Error::RuntimeConflict)?,
        )?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn browser_control(&self, token: &str, id: &str) -> Result<Option<BrowserControl>> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let org = identity.organization().as_str();
        let app = row(&mut tx, org, id).await?;
        let who = Caller {
            principal: identity.principal().as_str().into(),
            credential: crate::auth::token_id(token)?.into(),
            session: String::new(),
        };
        actor_access(&mut tx, org, &app, &who, RuntimePermission::Read, false).await?;
        reconcile_control(&mut tx, org, &app).await?;
        let result = control(&mut tx, org, id)
            .await?
            .as_ref()
            .map(control_view)
            .transpose()?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn submit_browser_command(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        input: &SubmitBrowserCommand,
    ) -> Result<BrowserCommandView> {
        let cmd = input.command.clone().normalize()?;
        if input.app_generation < 1 || input.control_epoch < 0 {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, cmd.permission().scope()).await?;
        let org = identity.organization().as_str();
        let app = row(&mut tx, org, id).await?;
        let who = authenticated(&identity, token, &input.connection_session_id)?;
        actor_access(&mut tx, org, &app, &who, cmd.permission(), true).await?;
        let hash = digest(
            "agent-computer/browser-command-v1",
            &(id, &who, input.app_generation, input.control_epoch, &cmd),
        )?;
        let op = "runtime.browser-command.v1";
        if let Some(old) = transactions::retry::<String>(&mut tx, &identity, op, key, &hash).await?
        {
            let mut result = request_view(&request(&mut tx, org, &old).await?)?;
            // Submitting again is status-only. Observation bytes require the read path.
            result.result = None;
            tx.commit().await?;
            return Ok(result);
        }
        reconcile_control(&mut tx, org, &app).await?;
        command_access(&mut tx, org, &app, &who, &cmd, input.control_epoch).await?;
        if app.try_get::<i64, _>("app_generation")? != input.app_generation {
            return Err(Error::RuntimeConflict);
        }
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM browser_requests WHERE organization=$1 AND instance_id=$2",
        )
        .bind(org)
        .bind(id)
        .fetch_one(&mut *tx)
        .await?;
        let pending:i64=sqlx::query_scalar("SELECT count(*) FROM browser_requests WHERE organization=$1 AND instance_id=$2 AND state IN ('Queued','Dispatched')").bind(org).bind(id).fetch_one(&mut *tx).await?;
        if total >= 1024 || pending >= 128 {
            return Err(Error::RuntimeBudgetExceeded);
        }
        let request_id = random_id("browser")?;
        let now = transactions::now(&mut tx).await?;
        let arm = require(&mut tx, org, id, "arm").await?;
        let expires = (now + 30_000).min(app.try_get("expires_at_ms")?).min(
            arm.payload["expires_at_ms"]
                .as_i64()
                .ok_or(Error::InvalidStoredData)?,
        );
        if expires <= now {
            return Err(Error::RuntimeConflict);
        }
        sqlx::query("INSERT INTO browser_requests(organization,request_id,instance_id,principal,credential_id,session_id,epoch,command,input_digest,state,created_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,'Queued',$10,$11)").bind(org).bind(&request_id).bind(id).bind(&who.principal).bind(&who.credential).bind(&who.session).bind(input.control_epoch).bind(serde_json::to_value(&cmd).map_err(|_|Error::InvalidStoredData)?).bind(&hash).bind(now).bind(expires).execute(&mut *tx).await?;
        transactions::save_receipt(&mut tx, &identity, op, key, &hash, &request_id).await?;
        transactions::emit(
            &mut tx,
            org,
            seq,
            "browser.command_queued",
            json!({"instance_id":id,"request_id":request_id,"epoch":input.control_epoch}),
        )
        .await?;
        command_access(&mut tx, org, &app, &who, &cmd, input.control_epoch).await?;
        let result = request_view(&request(&mut tx, org, &request_id).await?)?;
        tx.commit().await?;
        Ok(result)
    }
    pub async fn browser_command(&self, token: &str, id: &str) -> Result<BrowserCommandView> {
        let (mut tx, identity, _) = begin(self, token, ServiceScope::RuntimeRead).await?;
        let org = identity.organization().as_str();
        let r = request(&mut tx, org, id).await?;
        let who = caller(&r)?;
        if who.principal != identity.principal().as_str()
            || who.credential != crate::auth::token_id(token)?
        {
            return Err(Error::RuntimeAccessUnavailable);
        }
        let app = row(&mut tx, org, &r.try_get::<String, _>("instance_id")?).await?;
        actor_access(&mut tx, org, &app, &who, RuntimePermission::Read, false).await?;
        let cmd = command(&r)?;
        let mut result = request_view(&r)?;
        if matches!(cmd, BrowserCommand::Act { .. })
            && matches!(result.state.as_str(), "Dispatched" | "Unknown")
            && let Some(receipt) = worker::action_entry(&mut tx, org, id, "complete").await?
        {
            let typed: ActionReceipt =
                serde_json::from_value(receipt.clone()).map_err(|_| Error::InvalidStoredData)?;
            result.state = if typed.outcome == Outcome::Delivered {
                "Complete"
            } else {
                "Unknown"
            }
            .into();
            result.result = Some(json!({"receipt":receipt,"recorded":true}));
        }
        if !matches!(cmd, BrowserCommand::Act { .. })
            && result.state == "Complete"
            && result.result.is_some()
        {
            // Frames are not historical receipts: never disclose them under a
            // stale grant/session/epoch, even after a formerly successful read.
            command_access(&mut tx, org, &app, &who, &cmd, result.control_epoch).await?;
            if transactions::now(&mut tx).await?
                >= result.expires_at_ms.min(
                    r.try_get::<Option<i64>, _>("result_valid_until_ms")?
                        .ok_or(Error::InvalidStoredData)?,
                )
            {
                return Err(Error::RuntimeConflict);
            }
        }
        if result.state == "Queued"
            && (transactions::now(&mut tx).await? >= result.expires_at_ms
                || app.try_get::<String, _>("state")? == "Draining")
        {
            result.state = "Rejected".into();
        }
        if result.state == "Dispatched"
            && (transactions::now(&mut tx).await? >= result.expires_at_ms
                || app.try_get::<String, _>("state")? == "Draining")
        {
            // Presentation of uncertainty cannot recreate a dispatch permit.
            result.state = "Unknown".into();
        }
        tx.commit().await?;
        Ok(result)
    }
}
