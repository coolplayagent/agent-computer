use super::*;
use agent_computer_browser::ResponseObservation;

async fn claimed(
    tx: &mut Transaction<'_, Postgres>,
    attempt: &BrowserDispatch,
    work: &BrowserWork,
) -> Result<PgRow> {
    if work.instance != attempt.instance_id {
        return Err(Error::RuntimeConflict);
    }
    let r = request(tx, &attempt.organization, &work.id).await?;
    if r.try_get::<String, _>("state")? != "Dispatched"
        || r.try_get::<Option<String>, _>("dispatch_nonce")?.as_deref() != Some(&work.nonce)
    {
        return Err(Error::RuntimeConflict);
    }
    Ok(r)
}
async fn authorized(
    tx: &mut Transaction<'_, Postgres>,
    attempt: &BrowserDispatch,
    guard: &mut BrowserGuard,
    work: &BrowserWork,
) -> Result<u32> {
    let started = Instant::now();
    let r = claimed(tx, attempt, work).await?;
    let app = row(tx, &attempt.organization, &work.instance).await?;
    command_access(
        tx,
        &attempt.organization,
        &app,
        &work.caller,
        &work.command,
        work.epoch,
    )
    .await?;
    let now = transactions::now(tx).await?;
    let expiry: i64 = r.try_get("expires_at_ms")?;
    if now >= expiry {
        return Err(Error::RuntimeConflict);
    }
    let mut budget = live(tx, attempt, guard).await?.min((expiry - now) as u32);
    if matches!(work.command, BrowserCommand::Act { .. }) {
        let c = control(tx, &attempt.organization, &work.instance)
            .await?
            .ok_or(Error::RuntimeConflict)?;
        budget = budget.min(
            u32::try_from(c.try_get::<i64, _>("expires_at_ms")? - now)
                .map_err(|_| Error::RuntimeConflict)?,
        );
    }
    let budget = budget
        .min(30_000)
        .saturating_sub(u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX));
    if budget == 0 {
        return Err(Error::RuntimeAccessUnavailable);
    }
    Ok(budget)
}
pub(super) async fn action_entry(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    kind: &str,
) -> Result<Option<Value>> {
    let r=sqlx::query("SELECT payload,digest FROM browser_action_journal WHERE organization=$1 AND request_id=$2 AND kind=$3").bind(org).bind(id).bind(kind).fetch_optional(&mut **tx).await?;
    if let Some(r) = r {
        let v: Value = r.try_get("payload")?;
        if digest(
            "agent-computer/browser-action-journal-v1",
            &(org, id, kind, &v),
        )? != r.try_get::<String, _>("digest")?
        {
            return Err(Error::InvalidStoredData);
        }
        Ok(Some(v))
    } else {
        Ok(None)
    }
}
async fn write_entry(
    tx: &mut Transaction<'_, Postgres>,
    org: &str,
    id: &str,
    kind: &str,
    v: Value,
) -> Result<()> {
    let hash = digest(
        "agent-computer/browser-action-journal-v1",
        &(org, id, kind, &v),
    )?;
    sqlx::query("INSERT INTO browser_action_journal(organization,request_id,kind,payload,digest) VALUES($1,$2,$3,$4,$5)").bind(org).bind(id).bind(kind).bind(v).bind(hash).execute(&mut **tx).await?;
    Ok(())
}
impl Store {
    /// A lost claim acknowledgement consumes the request. Never adopt or replay.
    pub async fn next_browser_work(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
    ) -> Result<Option<BrowserWork>> {
        let started = Instant::now();
        let mut tx = self.pool.begin().await?;
        let org = &attempt.organization;
        let id = &attempt.instance_id;
        let seq = Self::lock_stream(&mut tx, org).await?;
        live(&mut tx, attempt, guard).await?;
        let app = row(&mut tx, org, id).await?;
        running(&mut tx, org, &app).await?;
        reconcile_control(&mut tx, org, &app).await?;
        if sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM browser_requests WHERE organization=$1 AND instance_id=$2 AND state='Dispatched')").bind(org).bind(id).fetch_one(&mut *tx).await? {return Err(Error::RuntimeConflict);}
        let Some(r)=sqlx::query("SELECT * FROM browser_requests WHERE organization=$1 AND instance_id=$2 AND state='Queued' ORDER BY sequence LIMIT 1").bind(org).bind(id).fetch_optional(&mut *tx).await? else{tx.commit().await?;return Ok(None);};
        let who = caller(&r)?;
        let cmd = command(&r)?;
        let wanted: i64 = r.try_get("epoch")?;
        let request_id: String = r.try_get("request_id")?;
        let check = command_access(&mut tx, org, &app, &who, &cmd, wanted).await;
        let expired = transactions::now(&mut tx).await? >= r.try_get("expires_at_ms")?;
        let rejected = match check {
            Ok(()) => expired,
            Err(
                Error::RuntimeConflict
                | Error::RuntimeAccessUnavailable
                | Error::Unauthenticated
                | Error::Forbidden
                | Error::ConnectionInactive
                | Error::ReferenceUnavailable,
            ) => true,
            Err(e) => return Err(e),
        };
        if rejected {
            sqlx::query("UPDATE browser_requests SET state='Rejected',result='{\"code\":\"authorization_expired\"}' WHERE organization=$1 AND request_id=$2").bind(org).bind(&request_id).execute(&mut *tx).await?;
            transactions::emit(
                &mut tx,
                org,
                seq,
                "browser.command_rejected",
                json!({"instance_id":id,"request_id":request_id}),
            )
            .await?;
            tx.commit().await?;
            return Ok(None);
        }
        let nonce = random_id("dispatch")?;
        sqlx::query("UPDATE browser_requests SET state='Dispatched',dispatch_nonce=$3 WHERE organization=$1 AND request_id=$2").bind(org).bind(&request_id).bind(&nonce).execute(&mut *tx).await?;
        transactions::emit(
            &mut tx,
            org,
            seq,
            "browser.command_dispatched",
            json!({"instance_id":id,"request_id":request_id}),
        )
        .await?;
        let work = BrowserWork {
            id: request_id,
            instance: id.clone(),
            nonce,
            caller: who,
            epoch: wanted,
            command: cmd,
            started,
        };
        authorized(&mut tx, attempt, guard, &work).await?;
        tx.commit().await?;
        Ok(Some(work))
    }
    pub async fn browser_callback(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
        work: &BrowserWork,
        callback: &Callback,
    ) -> Result<Decision> {
        let mut tx = self.pool.begin().await?;
        let org = &attempt.organization;
        let seq = Self::lock_stream(&mut tx, org).await?;
        claimed(&mut tx, attempt, work).await?;
        let decision = match callback {
            Callback::Authorize(r) => {
                let permission = if matches!(work.command, BrowserCommand::Act { .. }) {
                    Permission::Control
                } else {
                    Permission::Observe
                };
                if r.scope != attempt.plan.binding().scope
                    || r.actor != work.caller.actor()
                    || r.permission != permission
                {
                    return Err(Error::RuntimeAccessUnavailable);
                }
                let budget = authorized(&mut tx, attempt, guard, work).await?;
                let before = Instant::now();
                tx.commit().await?;
                let budget = budget
                    .saturating_sub(u32::try_from(before.elapsed().as_millis()).unwrap_or(u32::MAX))
                    .min(attempt.remaining_budget_ms()?)
                    .min(
                        guard
                            .remaining_budget_ms()
                            .map_err(|_| Error::RuntimeAccessUnavailable)?,
                    );
                if budget == 0 {
                    return Err(Error::RuntimeAccessUnavailable);
                }
                return Ok(Decision::Grant(Grant {
                    scope: r.scope.clone(),
                    actor: r.actor.clone(),
                    permission,
                    control_epoch: work.epoch as u64,
                    valid_for_ms: budget,
                }));
            }
            Callback::Begin(intent) => {
                let BrowserCommand::Act { action } = &work.command else {
                    return Err(Error::RuntimeConflict);
                };
                if intent.scope != attempt.plan.binding().scope
                    || intent.actor != work.caller.actor()
                    || intent.action_id != work.id
                    || intent.control_epoch != work.epoch as u64
                    || intent.action != *action
                    || !valid_digest(&intent.input_digest)
                {
                    return Err(Error::RuntimeConflict);
                }
                authorized(&mut tx, attempt, guard, work).await?;
                if action_entry(&mut tx, org, &work.id, "begin")
                    .await?
                    .is_some()
                {
                    return Err(Error::DispatchAlreadyStarted);
                }
                write_entry(
                    &mut tx,
                    org,
                    &work.id,
                    "begin",
                    serde_json::to_value(intent).map_err(|_| Error::InvalidStoredData)?,
                )
                .await?;
                transactions::emit(
                    &mut tx,
                    org,
                    seq,
                    "browser.action_intent",
                    json!({"instance_id":work.instance,"request_id":work.id,"epoch":work.epoch}),
                )
                .await?;
                authorized(&mut tx, attempt, guard, work).await?;
                Decision::Dispatch
            }
            Callback::Complete(receipt) => {
                let intent: ActionIntent = serde_json::from_value(
                    action_entry(&mut tx, org, &work.id, "begin")
                        .await?
                        .ok_or(Error::RuntimeConflict)?,
                )
                .map_err(|_| Error::InvalidStoredData)?;
                if receipt.action_id != work.id
                    || receipt.input_digest != intent.input_digest
                    || receipt.app_instance_id != work.instance
                    || receipt.app_generation != intent.scope.app_generation
                    || receipt.control_epoch != work.epoch as u64
                    || receipt
                        .page_id
                        .as_deref()
                        .is_some_and(|s| !agent_computer_browser::identifier(s))
                {
                    return Err(Error::RuntimeConflict);
                }
                let value = serde_json::to_value(receipt).map_err(|_| Error::InvalidStoredData)?;
                if let Some(old) = action_entry(&mut tx, org, &work.id, "complete").await? {
                    if old != value {
                        return Err(Error::RuntimeConflict);
                    }
                } else {
                    write_entry(&mut tx, org, &work.id, "complete", value).await?;
                    transactions::emit(&mut tx,org,seq,"browser.action_receipt",json!({"instance_id":work.instance,"request_id":work.id,"outcome":receipt.outcome,"business_success_confirmed":false})).await?;
                }
                Decision::Recorded
            }
        };
        tx.commit().await?;
        Ok(decision)
    }
    /// Store a validated, correlated response. Missing completion never replays.
    pub async fn finish_browser_work(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
        work: BrowserWork,
        response: Option<&ResponseObservation>,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let org = &attempt.organization;
        let seq = Self::lock_stream(&mut tx, org).await?;
        claimed(&mut tx, attempt, &work).await?;
        if response.is_some_and(|r| {
            r.scope() != &attempt.plan.binding().scope || r.operation() != &work.operation()
        }) {
            return Err(Error::RuntimeConflict);
        }
        let mut valid_until = None;
        let (state, value) = if matches!(work.command, BrowserCommand::Act { .. }) {
            if let Some(v) = action_entry(&mut tx, org, &work.id, "complete").await? {
                let receipt: ActionReceipt =
                    serde_json::from_value(v.clone()).map_err(|_| Error::InvalidStoredData)?;
                (
                    if receipt.outcome == Outcome::Delivered {
                        "Complete"
                    } else {
                        "Unknown"
                    },
                    json!({"receipt":v,"recorded":true}),
                )
            } else if action_entry(&mut tx, org, &work.id, "begin")
                .await?
                .is_some()
            {
                ("Unknown", json!({"code":"receipt_unknown"}))
            } else {
                ("Rejected", json!({"code":"not_admitted"}))
            }
        } else if let Some(response) = response.filter(|r| r.ok()) {
            authorized(&mut tx, attempt, guard, &work).await?;
            let size:i64=sqlx::query_scalar("SELECT COALESCE(sum(octet_length(result::text)),0)::bigint FROM browser_requests WHERE organization=$1 AND instance_id=$2").bind(org).bind(&work.instance).fetch_one(&mut *tx).await?;
            let value = response.value().clone();
            let bytes = serde_json::to_vec(&value)
                .map_err(|_| Error::InvalidStoredData)?
                .len();
            if bytes > 3 * 1024 * 1024 || size + bytes as i64 > 64 * 1024 * 1024 {
                return Err(Error::RuntimeBudgetExceeded);
            }
            let remaining = if matches!(work.command, BrowserCommand::Observe { .. }) {
                let initial = value["valid_for_ms"]
                    .as_u64()
                    .ok_or(Error::InvalidStoredData)?;
                initial.saturating_sub(
                    u64::try_from(work.started.elapsed().as_millis()).unwrap_or(u64::MAX),
                )
            } else {
                30_000
            };
            if remaining == 0 {
                return Err(Error::RuntimeConflict);
            }
            valid_until = Some(transactions::now(&mut tx).await? + remaining.min(30_000) as i64);
            ("Complete", value)
        } else {
            ("Rejected", json!({"code":"observation_unavailable"}))
        };
        sqlx::query("UPDATE browser_requests SET state=$3,result=$4,result_valid_until_ms=$5 WHERE organization=$1 AND request_id=$2").bind(org).bind(&work.id).bind(state).bind(value).bind(valid_until).execute(&mut *tx).await?;
        if state == "Unknown" {
            sqlx::query("UPDATE browser_controls SET state='Blocked' WHERE organization=$1 AND instance_id=$2 AND state IN ('Owned','Draining')").bind(org).bind(&work.instance).execute(&mut *tx).await?;
        }
        transactions::emit(
            &mut tx,
            org,
            seq,
            "browser.command_completed",
            json!({"instance_id":work.instance,"request_id":work.id,"state":state}),
        )
        .await?;
        tx.commit().await?;
        Ok(state != "Unknown")
    }
    pub async fn next_browser_drain(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
    ) -> Result<Option<BrowserDrain>> {
        let mut tx = self.pool.begin().await?;
        let org = &attempt.organization;
        let id = &attempt.instance_id;
        Self::lock_stream(&mut tx, org).await?;
        live(&mut tx, attempt, guard).await?;
        let app = row(&mut tx, org, id).await?;
        reconcile_control(&mut tx, org, &app).await?;
        let Some(c) = control(&mut tx, org, id).await? else {
            tx.commit().await?;
            return Ok(None);
        };
        if c.try_get::<String, _>("state")? == "Blocked" {
            return Err(Error::RuntimeConflict);
        }
        if c.try_get::<String, _>("state")? != "Draining" {
            tx.commit().await?;
            return Ok(None);
        }
        let unresolved:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM browser_requests WHERE organization=$1 AND instance_id=$2 AND state IN ('Dispatched','Unknown'))").bind(org).bind(id).fetch_one(&mut *tx).await?;
        if unresolved || c.try_get::<Option<String>, _>("drain_nonce")?.is_some() {
            return Err(Error::RuntimeConflict);
        }
        let nonce = random_id("drain")?;
        sqlx::query(
            "UPDATE browser_controls SET drain_nonce=$3 WHERE organization=$1 AND instance_id=$2",
        )
        .bind(org)
        .bind(id)
        .bind(&nonce)
        .execute(&mut *tx)
        .await?;
        let value = BrowserDrain {
            epoch: c.try_get("epoch")?,
            instance: id.clone(),
            nonce,
        };
        tx.commit().await?;
        Ok(Some(value))
    }
    pub async fn finish_browser_drain(
        &self,
        attempt: &BrowserDispatch,
        guard: &mut BrowserGuard,
        drain: BrowserDrain,
        response: &ResponseObservation,
    ) -> Result<()> {
        if response.scope() != &attempt.plan.binding().scope
            || response.operation()
                != &(agent_computer_browser::Operation::Drain {
                    control_epoch: drain.epoch as u64,
                })
        {
            return Err(Error::RuntimeConflict);
        }
        if drain.instance != attempt.instance_id
            || !response.ok()
            || response.value()
                != &json!({"control_epoch":drain.epoch,"drained":true,"requires_observation":true})
        {
            return Err(Error::RuntimeConflict);
        }
        let mut tx = self.pool.begin().await?;
        let org = &attempt.organization;
        let seq = Self::lock_stream(&mut tx, org).await?;
        live(&mut tx, attempt, guard).await?;
        let n=sqlx::query("UPDATE browser_controls SET state='Idle' WHERE organization=$1 AND instance_id=$2 AND state='Draining' AND epoch=$3 AND drain_nonce=$4").bind(org).bind(&drain.instance).bind(drain.epoch).bind(&drain.nonce).execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(Error::RuntimeConflict);
        }
        transactions::emit(
            &mut tx,
            org,
            seq,
            "browser.control_drained",
            json!({"instance_id":drain.instance,"epoch":drain.epoch,"requires_observation":true}),
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }
}
fn valid_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|v| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}
