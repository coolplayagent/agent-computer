use crate::{Error, Precondition, Receipt, RecordDeclaration, Result, Store};
use sha2::{Digest, Sha256};
use sqlx::Row;

pub(crate) const OPERATION: &str = "declaration_record_v1";

impl Store {
    /// Commit an immutable declaration version, its retry receipt and event/Outbox.
    /// This is a registry write, not an authorized ComputerSet apply or runtime job.
    pub async fn record(&self, request: RecordDeclaration<'_>) -> Result<Receipt> {
        let expected = match request.precondition {
            Precondition::Create => 0_i64,
            Precondition::Match(revision) => i64::try_from(revision.value())
                .ok()
                .filter(|v| *v > 0)
                .ok_or(Error::InvalidPrecondition)?,
        };
        let org = request.organization.as_str();
        let name = &request.definition.document().metadata.name;
        let digest = request
            .definition
            .report()
            .definition_digest
            .as_deref()
            .ok_or(Error::InvalidStoredData)?;
        let canonical = request.definition.canonical_bytes();
        let mut hash = Sha256::new();
        hash.update(OPERATION.as_bytes());
        hash.update([0]);
        hash.update(expected.to_be_bytes());
        hash.update(canonical); // Includes target name and every declaration field.
        let input = hash.finalize().to_vec();

        let mut tx = self.pool.begin().await?;
        let sequence = Self::lock_stream(&mut tx, org).await?;
        let previous = sqlx::query("SELECT input_digest, response, retired FROM request_records WHERE organization=$1 AND principal=$2 AND operation=$3 AND request_key=$4")
            .bind(org).bind(request.principal.as_str()).bind(OPERATION).bind(request.key.as_str())
            .fetch_optional(&mut *tx).await?;
        if let Some(previous) = previous {
            if previous.try_get::<bool, _>("retired")? {
                return Err(Error::IdempotencyGone);
            }
            if previous.try_get::<Vec<u8>, _>("input_digest")? != input {
                return Err(Error::IdempotencyConflict);
            }
            let response: serde_json::Value = previous.try_get("response")?;
            let receipt = serde_json::from_value(response).map_err(|_| Error::InvalidStoredData)?;
            tx.commit().await?;
            return Ok(receipt);
        }
        let actual: Option<i64> = sqlx::query_scalar(
            "SELECT revision FROM declaration_heads WHERE organization=$1 AND name=$2",
        )
        .bind(org)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await?;
        if actual.unwrap_or(0) != expected {
            return Err(Error::RevisionConflict);
        }
        let revision = expected.checked_add(1).ok_or(Error::CounterExhausted)?;
        let sequence = sequence.checked_add(1).ok_or(Error::CounterExhausted)?;
        let receipt = Receipt {
            name: name.clone(),
            revision,
            digest: digest.into(),
            event_sequence: sequence,
        };
        let response = serde_json::to_value(&receipt).map_err(|_| Error::InvalidStoredData)?;

        sqlx::query("INSERT INTO declaration_heads (organization,name,revision) VALUES ($1,$2,$3) ON CONFLICT (organization,name) DO UPDATE SET revision=EXCLUDED.revision")
            .bind(org).bind(name).bind(revision).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO declaration_versions (organization,name,revision,digest,canonical) VALUES ($1,$2,$3,$4,$5)")
            .bind(org).bind(name).bind(revision).bind(digest).bind(canonical).execute(&mut *tx).await?;
        sqlx::query("UPDATE organization_streams SET last_sequence=$2 WHERE organization=$1")
            .bind(org)
            .bind(sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO events (organization,sequence,kind,payload) VALUES ($1,$2,'declaration.recorded',$3)")
            .bind(org).bind(sequence).bind(&response).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO outbox (organization,sequence) VALUES ($1,$2)")
            .bind(org)
            .bind(sequence)
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO request_records (organization,principal,operation,request_key,input_digest,response) VALUES ($1,$2,$3,$4,$5,$6)")
            .bind(org).bind(request.principal.as_str()).bind(OPERATION).bind(request.key.as_str())
            .bind(input).bind(response).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(receipt)
    }
}
