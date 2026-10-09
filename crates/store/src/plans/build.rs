use super::{access, references, transactions, types::*};
use crate::{Error, Result, Store, auth::ServiceScope};
use agent_computer_core::identity::IdempotencyKey;
use agent_computer_definitions::ValidatedDefinition;
use serde_json::Value;
use sqlx::Row;

pub(super) const MAX_PLAN_BYTES: usize = 8 * 1024 * 1024;
pub(super) const PLAN_OPERATION: &str = "definitions.plan.v1";

impl Store {
    pub async fn create_definition_plan(
        &self,
        token: &str,
        key: &IdempotencyKey,
        definition: &ValidatedDefinition,
    ) -> Result<DefinitionPlan> {
        let (mut tx, identity, seq) = transactions::begin(self, token).await?;
        let input = definition
            .report()
            .definition_digest
            .as_deref()
            .ok_or(Error::InvalidStoredData)?;
        if let Some(plan) =
            transactions::retry::<DefinitionPlan>(&mut tx, &identity, PLAN_OPERATION, key, input)
                .await?
        {
            access::authorize_plan(&mut tx, &identity, &plan).await?;
            Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
            tx.commit().await?;
            return Ok(plan);
        }
        let org = identity.organization().as_str();
        let metadata = &definition.document().metadata;
        let current: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM declaration_heads WHERE organization=$1 AND name=$2",
        )
        .bind(org)
        .bind(&metadata.name)
        .fetch_optional(&mut *tx)
        .await?;
        let previous = current.unwrap_or(0);
        access::require(
            &mut tx,
            &identity,
            DefinitionKind::Declaration,
            &metadata.name,
            if previous == 0 {
                DefinitionPermission::Create
            } else {
                DefinitionPermission::Manage
            },
        )
        .await?;
        expected(metadata.expected_revision, previous)?;
        let document =
            serde_json::to_value(definition.document()).map_err(|_| Error::InvalidStoredData)?;
        let mut resources = vec![];
        let mut bytes = 0;
        // The reference graph is typed and acyclic in this order. Names resolve to
        // the versions selected by this plan, never a later mutable head.
        for (collection, kind) in [
            ("volumes", DefinitionKind::Volume),
            ("workspaces", DefinitionKind::Workspace),
            ("sandboxes", DefinitionKind::Sandbox),
            ("apps", DefinitionKind::App),
            ("agents", DefinitionKind::Agent),
            ("computers", DefinitionKind::Computer),
        ] {
            for source in document["spec"][collection]
                .as_array()
                .ok_or(Error::InvalidStoredData)?
            {
                let name = source["name"].as_str().ok_or(Error::InvalidStoredData)?;
                let row=sqlx::query("SELECT d.resource_id,d.revision,v.digest,v.spec,v.dependencies FROM resource_definitions d JOIN resource_spec_versions v USING (organization,resource_id,revision) WHERE d.organization=$1 AND d.kind=$2 AND d.name=$3")
                    .bind(org).bind(kind.as_str()).bind(name).fetch_optional(&mut *tx).await?;
                let actual = row
                    .as_ref()
                    .map(|r| r.try_get::<i64, _>("revision"))
                    .transpose()?
                    .unwrap_or(0);
                access::require(
                    &mut tx,
                    &identity,
                    kind,
                    name,
                    if actual == 0 {
                        DefinitionPermission::Create
                    } else {
                        DefinitionPermission::Manage
                    },
                )
                .await?;
                expected(source["expectedRevision"].as_u64(), actual)?;
                let mut after = source.clone();
                let object = after.as_object_mut().ok_or(Error::InvalidStoredData)?;
                object.remove("name");
                object.remove("expectedRevision");
                let dependencies =
                    references::bind(&mut tx, &identity, kind, &mut after, &resources).await?;
                let hash = digest(
                    "agent-computer/resource-spec-v1",
                    &(kind, &after, &dependencies),
                )?;
                let (id, before, before_digest, before_dependencies) = match row {
                    Some(row) => (
                        row.try_get("resource_id")?,
                        Some(row.try_get::<Value, _>("spec")?),
                        Some(row.try_get::<String, _>("digest")?),
                        serde_json::from_value(row.try_get("dependencies")?)
                            .map_err(|_| Error::InvalidStoredData)?,
                    ),
                    None => (random_id("res")?, None, None, vec![]),
                };
                if let Some(old) = &before
                    && ((kind == DefinitionKind::Volume
                        && (old["storageClass"] != after["storageClass"]
                            || after["quotaBytes"].as_u64() < old["quotaBytes"].as_u64()))
                        || (kind == DefinitionKind::Workspace
                            && old["volumeRef"] != after["volumeRef"]))
                {
                    return Err(Error::UnsupportedChange);
                }
                let change = if actual == 0 {
                    Change::Create
                } else if before_digest.as_deref() == Some(&hash) {
                    Change::Unchanged
                } else {
                    Change::Update
                };
                let revision = if change == Change::Unchanged {
                    actual
                } else {
                    actual.checked_add(1).ok_or(Error::CounterExhausted)?
                };
                let hosted_agent = kind == DefinitionKind::Agent
                    && (after["mode"] == "hosted"
                        || before.as_ref().is_some_and(|old| old["mode"] == "hosted"));
                let requires_drain = change == Change::Update
                    && (matches!(
                        kind,
                        DefinitionKind::Sandbox | DefinitionKind::App | DefinitionKind::Computer
                    ) || hosted_agent);
                let resource = PlannedResource {
                    kind,
                    name: name.into(),
                    resource_id: id,
                    expected_revision: actual,
                    revision,
                    digest: hash,
                    change,
                    before,
                    before_digest,
                    before_dependencies,
                    after,
                    dependencies,
                    requires_drain,
                };
                references::compatibility(&mut tx, org, &resource, &resources).await?;
                bytes += serde_json::to_vec(&resource)
                    .map_err(|_| Error::InvalidStoredData)?
                    .len();
                if bytes > MAX_PLAN_BYTES {
                    return Err(Error::PlanTooLarge);
                }
                resources.push(resource);
            }
        }
        references::authorize_tree(&mut tx, &identity, &resources).await?;
        let mut plan = DefinitionPlan {
            plan_id: random_id("plan")?,
            plan_digest: String::new(),
            organization: org.into(),
            principal: identity.principal().as_str().into(),
            declaration_name: metadata.name.clone(),
            declaration_expected_revision: previous,
            definition_digest: input.into(),
            expires_at_ms: transactions::now(&mut tx)
                .await?
                .checked_add(900_000)
                .ok_or(Error::CounterExhausted)?,
            resources,
            deletes_data: false,
            event_sequence: seq.checked_add(1).ok_or(Error::CounterExhausted)?,
        };
        plan.plan_digest = digest("agent-computer/definition-plan-v1", &plan)?;
        let preview = serde_json::to_value(&plan).map_err(|_| Error::InvalidStoredData)?;
        if serde_json::to_vec(&preview)
            .map_err(|_| Error::InvalidStoredData)?
            .len()
            > MAX_PLAN_BYTES
        {
            return Err(Error::PlanTooLarge);
        }
        sqlx::query("INSERT INTO definition_plans (organization,plan_id,principal,digest,preview,canonical,expires_at_ms) VALUES ($1,$2,$3,$4,$5,$6,$7)")
            .bind(org).bind(&plan.plan_id).bind(identity.principal().as_str()).bind(&plan.plan_digest).bind(preview).bind(definition.canonical_bytes()).bind(plan.expires_at_ms).execute(&mut *tx).await?;
        transactions::emit(
            &mut tx,
            org,
            seq,
            "definition.plan_created",
            serde_json::json!({"plan_id":plan.plan_id,"plan_digest":plan.plan_digest}),
        )
        .await?;
        transactions::save_receipt(&mut tx, &identity, PLAN_OPERATION, key, input, &plan).await?;
        Store::authorize_service_in(&mut tx, token, ServiceScope::DefinitionsManage).await?;
        tx.commit().await?;
        Ok(plan)
    }
}
