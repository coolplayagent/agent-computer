use super::{access, references, transactions, types::*};
use crate::{
    Error, Result, Store,
    auth::{AuthenticatedPrincipal, ServiceScope},
};
use agent_computer_core::identity::IdempotencyKey;
use sqlx::{Postgres, Row, Transaction};

const APPLY_OPERATION: &str = "definitions.apply.v1";

pub(crate) async fn load_plan(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    id: &str,
) -> Result<(DefinitionPlan, Vec<u8>)> {
    let row=sqlx::query("SELECT preview,canonical FROM definition_plans WHERE organization=$1 AND principal=$2 AND plan_id=$3")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(id).fetch_optional(&mut **tx).await?.ok_or(Error::PlanNotFound)?;
    let plan: DefinitionPlan =
        serde_json::from_value(row.try_get("preview")?).map_err(|_| Error::InvalidStoredData)?;
    let mut verified = plan.clone();
    verified.plan_digest.clear();
    if digest("agent-computer/definition-plan-v1", &verified)? != plan.plan_digest {
        return Err(Error::InvalidStoredData);
    }
    Ok((plan, row.try_get("canonical")?))
}

async fn operation(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    plan: &DefinitionPlan,
) -> Result<Option<DefinitionOperation>> {
    let row=sqlx::query("SELECT operation_id,state,event_sequence FROM operations WHERE organization=$1 AND principal=$2 AND plan_id=$3")
        .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(&plan.plan_id).fetch_optional(&mut **tx).await?;
    let Some(row) = row else { return Ok(None) };
    let operation_id: String = row.try_get("operation_id")?;
    let progress =
        crate::reconciliation::progress(tx, identity.organization().as_str(), &operation_id)
            .await?;
    let watermark =
        sqlx::query_scalar("SELECT last_sequence FROM organization_streams WHERE organization=$1")
            .bind(identity.organization().as_str())
            .fetch_one(&mut **tx)
            .await?;
    Ok(Some(DefinitionOperation {
        operation_id,
        plan_id: plan.plan_id.clone(),
        state: row.try_get("state")?,
        resources: plan.resources.iter().map(references::dependency).collect(),
        event_sequence: row.try_get("event_sequence")?,
        progress,
        watermark,
    }))
}

impl Store {
    pub async fn definition_plan(&self, token: &str, id: &str) -> Result<DefinitionPlan> {
        let (mut tx, identity, _) = transactions::begin(self, token).await?;
        let (plan, _) = load_plan(&mut tx, &identity, id).await?;
        access::authorize_plan(&mut tx, &identity, &plan).await?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
        tx.commit().await?;
        Ok(plan)
    }

    pub async fn definition_operation(&self, token: &str, id: &str) -> Result<DefinitionOperation> {
        let (mut tx, identity, _) = transactions::begin(self, token).await?;
        let plan_id:String=sqlx::query_scalar("SELECT plan_id FROM operations WHERE organization=$1 AND principal=$2 AND operation_id=$3")
            .bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(id).fetch_optional(&mut *tx).await?.ok_or(Error::PlanNotFound)?;
        let (plan, _) = load_plan(&mut tx, &identity, &plan_id).await?;
        access::authorize_plan(&mut tx, &identity, &plan).await?;
        let result = operation(&mut tx, &identity, &plan)
            .await?
            .ok_or(Error::InvalidStoredData)?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
        tx.commit().await?;
        Ok(result)
    }

    pub async fn apply_definition_plan(
        &self,
        token: &str,
        key: &IdempotencyKey,
        id: &str,
        plan_digest: &str,
    ) -> Result<DefinitionOperation> {
        let (mut tx, identity, seq) = transactions::begin(self, token).await?;
        let input = digest("agent-computer/apply-request-v1", &(id, plan_digest))?;
        let retried = transactions::retry::<DefinitionOperation>(
            &mut tx,
            &identity,
            APPLY_OPERATION,
            key,
            &input,
        )
        .await?;
        let (plan, canonical) = load_plan(&mut tx, &identity, id).await?;
        if plan.plan_digest != plan_digest {
            return Err(Error::PlanDigestMismatch);
        }
        access::authorize_plan(&mut tx, &identity, &plan).await?;
        if let Some(existing) = operation(&mut tx, &identity, &plan).await? {
            if retried.is_none() {
                transactions::save_receipt(
                    &mut tx,
                    &identity,
                    APPLY_OPERATION,
                    key,
                    &input,
                    &existing,
                )
                .await?;
            }
            Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
            tx.commit().await?;
            return Ok(existing);
        }
        if retried.is_some() {
            return Err(Error::InvalidStoredData);
        }
        if transactions::now(&mut tx).await? >= plan.expires_at_ms {
            return Err(Error::PlanExpired);
        }
        let org = identity.organization().as_str();
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM declaration_heads WHERE organization=$1 AND name=$2",
        )
        .bind(org)
        .bind(&plan.declaration_name)
        .fetch_optional(&mut *tx)
        .await?;
        if current.unwrap_or(0) != plan.declaration_expected_revision {
            return Err(Error::RevisionConflict);
        }
        for resource in &plan.resources {
            let row=sqlx::query("SELECT resource_id,revision FROM resource_definitions WHERE organization=$1 AND kind=$2 AND name=$3")
                .bind(org).bind(resource.kind.as_str()).bind(&resource.name).fetch_optional(&mut *tx).await?;
            if let Some(row) = row {
                if row.try_get::<String, _>("resource_id")? != resource.resource_id
                    || row.try_get::<i64, _>("revision")? != resource.expected_revision
                {
                    return Err(Error::RevisionConflict);
                }
            } else if resource.expected_revision != 0 {
                return Err(Error::RevisionConflict);
            }
        }
        references::check_dependencies(&mut tx, &identity, &plan).await?;
        let revision = plan
            .declaration_expected_revision
            .checked_add(1)
            .ok_or(Error::CounterExhausted)?;
        sqlx::query("INSERT INTO declaration_heads (organization,name,revision) VALUES ($1,$2,$3) ON CONFLICT (organization,name) DO UPDATE SET revision=EXCLUDED.revision")
            .bind(org).bind(&plan.declaration_name).bind(revision).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO declaration_versions (organization,name,revision,digest,canonical) VALUES ($1,$2,$3,$4,$5)")
            .bind(org).bind(&plan.declaration_name).bind(revision).bind(&plan.definition_digest).bind(canonical).execute(&mut *tx).await?;
        creator_grant(
            &mut tx,
            &identity,
            DefinitionKind::Declaration,
            &plan.declaration_name,
            plan.declaration_expected_revision == 0,
        )
        .await?;
        let operation_id = random_id("op")?;
        sqlx::query("INSERT INTO operations (organization,operation_id,principal,plan_id,event_sequence,credential_id) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(org).bind(&operation_id).bind(identity.principal().as_str()).bind(id).bind(seq.checked_add(1).ok_or(Error::CounterExhausted)?).bind(crate::auth::token_id(token)?).execute(&mut *tx).await?;
        for (ordinal, resource) in plan.resources.iter().enumerate() {
            if resource.change != Change::Unchanged {
                sqlx::query("INSERT INTO resource_definitions (organization,resource_id,kind,name,revision) VALUES ($1,$2,$3,$4,$5) ON CONFLICT (organization,resource_id) DO UPDATE SET revision=EXCLUDED.revision")
                    .bind(org).bind(&resource.resource_id).bind(resource.kind.as_str()).bind(&resource.name).bind(resource.revision).execute(&mut *tx).await?;
                sqlx::query("INSERT INTO resource_spec_versions (organization,resource_id,revision,digest,spec,dependencies) VALUES ($1,$2,$3,$4,$5,$6)")
                    .bind(org).bind(&resource.resource_id).bind(resource.revision).bind(&resource.digest).bind(&resource.after).bind(serde_json::to_value(&resource.dependencies).map_err(|_|Error::InvalidStoredData)?).execute(&mut *tx).await?;
            }
            creator_grant(
                &mut tx,
                &identity,
                resource.kind,
                &resource.name,
                resource.expected_revision == 0,
            )
            .await?;
            sqlx::query("INSERT INTO reconcile_intents (organization,operation_id,ordinal,resource_id,revision,requires_drain,step_id,event_sequence) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
                .bind(org).bind(&operation_id).bind(ordinal as i32).bind(&resource.resource_id).bind(resource.revision).bind(resource.requires_drain).bind(random_id("step")?).bind(seq.checked_add(1).ok_or(Error::CounterExhausted)?).execute(&mut *tx).await?;
        }
        let event_sequence=transactions::emit(&mut tx,org,seq,"definition.applied",serde_json::json!({"operation_id":operation_id,"plan_id":id,"declaration_name":plan.declaration_name,"revision":revision})).await?;
        let response = DefinitionOperation {
            watermark: event_sequence,
            progress: crate::reconciliation::progress(&mut tx, org, &operation_id).await?,
            operation_id,
            plan_id: id.into(),
            state: "Queued".into(),
            resources: plan.resources.iter().map(references::dependency).collect(),
            event_sequence,
        };
        transactions::save_receipt(&mut tx, &identity, APPLY_OPERATION, key, &input, &response)
            .await?;
        // Keep admission valid at commit, including credentials that expired while
        // the transaction waited. Revocation/disable cannot pass our row locks.
        Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
        tx.commit().await?;
        Ok(response)
    }
}

async fn creator_grant(
    tx: &mut Transaction<'_, Postgres>,
    identity: &AuthenticatedPrincipal,
    kind: DefinitionKind,
    name: &str,
    created: bool,
) -> Result<()> {
    if created {
        sqlx::query("INSERT INTO definition_grants (organization,principal,kind,name,permission) VALUES ($1,$2,$3,$4,'manage') ON CONFLICT DO NOTHING").bind(identity.organization().as_str()).bind(identity.principal().as_str()).bind(kind.as_str()).bind(name).execute(&mut **tx).await?;
    }
    Ok(())
}
