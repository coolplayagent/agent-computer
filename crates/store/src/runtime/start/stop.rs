use super::*;

impl Store {
    /// This path never kills processes or discards writes: any recorded user
    /// dispatch blocks it, including completed bounded-file edits in old epochs.
    pub async fn stop_prepared_computer(
        &self,
        token: &str,
        key: &IdempotencyKey,
        computer: &str,
        request: &StopPreparedComputer,
    ) -> Result<ComputerStopReceipt> {
        if request.expected_revision < 1 || ComputerId::new(&request.request_id).is_err() {
            return Err(Error::InvalidRuntimeRequest);
        }
        let (mut tx, identity, seq) = begin(self, token, ServiceScope::RuntimeManage).await?;
        let requirements = [requirement(computer, RuntimePermission::Manage, None)];
        authorize_in(&mut tx, token, &requirements).await?;
        let org = identity.organization().as_str();
        let op = "runtime.stop-prepared.v1";
        let input = digest(
            "agent-computer/stop-prepared-input-v1",
            &(computer, request),
        )?;
        if let Some(receipt) = transactions::retry(&mut tx, &identity, op, key, &input).await? {
            authorize_in(&mut tx, token, &requirements).await?;
            tx.commit().await?;
            return Ok(receipt);
        }
        let current = state(&mut tx, org, computer).await?;
        if current.revision != request.expected_revision
            || current.active_request.as_deref() != Some(&request.request_id)
            || current.start_state != Some(StartState::Prepared)
        {
            return Err(Error::RuntimeConflict);
        }
        // The organization stream lock serializes stop with grants, sessions,
        // writer acquisition, preparation and user dispatch. Use all epochs.
        let clean: bool = sqlx::query_scalar("SELECT runtime_stop_is_undispatched($1,$2)")
            .bind(org)
            .bind(&request.request_id)
            .fetch_one(&mut *tx)
            .await?;
        if !clean {
            return Err(Error::RuntimeStopBlocked);
        }
        let source = sqlx::query("SELECT r.candidate_id,r.storage_bytes,i.revision,v.digest FROM runtime_start_requests r JOIN runtime_start_inputs i USING(organization,request_id) JOIN workspace_input_versions v ON v.organization=i.organization AND v.workspace_id=i.workspace_id AND v.revision=i.revision WHERE r.organization=$1 AND r.request_id=$2")
            .bind(org).bind(&request.request_id).fetch_one(&mut *tx).await?;
        let receipt = ComputerStopReceipt {
            computer_id: computer.into(),
            request_id: request.request_id.clone(),
            generation: current.generation,
            candidate_id: source.try_get("candidate_id")?,
            control_revision: current
                .revision
                .checked_add(1)
                .ok_or(Error::CounterExhausted)?,
            input_revision: source.try_get("revision")?,
            input_manifest_digest: source.try_get("digest")?,
            retained_storage_bytes: source.try_get("storage_bytes")?,
            proof: "no_user_dispatch".into(),
            stopped_at_ms: transactions::now(&mut tx).await?,
            event_sequence: seq,
        };
        let value = serde_json::to_value(&receipt).map_err(|_| Error::InvalidStoredData)?;
        sqlx::query(
            "INSERT INTO runtime_stops (organization,request_id,receipt) VALUES ($1,$2,$3)",
        )
        .bind(org)
        .bind(&request.request_id)
        .bind(&value)
        .execute(&mut *tx)
        .await?;
        sqlx::query("UPDATE runtime_start_requests SET state='Stopped' WHERE organization=$1 AND request_id=$2")
            .bind(org).bind(&request.request_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE runtime_controls SET revision=$3,active_request=NULL WHERE organization=$1 AND computer_id=$2")
            .bind(org).bind(computer).bind(receipt.control_revision).execute(&mut *tx).await?;
        transactions::emit(&mut tx, org, seq, "computer.stopped", value).await?;
        transactions::save_receipt(&mut tx, &identity, op, key, &input, &receipt).await?;
        authorize_in(&mut tx, token, &requirements).await?;
        tx.commit().await?;
        Ok(receipt)
    }
}
