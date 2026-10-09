use crate::{DeclarationVersion, Error, Event, EventPage, Result, Snapshot, Store};
use agent_computer_core::identity::OrganizationId;
use sqlx::{Row, postgres::PgRow};

pub(crate) fn event(row: PgRow) -> Result<Event> {
    Ok(Event {
        sequence: row.try_get("sequence")?,
        kind: row.try_get("kind")?,
        payload: row.try_get("payload")?,
    })
}

fn version(row: PgRow) -> Result<DeclarationVersion> {
    Ok(DeclarationVersion {
        name: row.try_get("name")?,
        revision: row.try_get("revision")?,
        digest: row.try_get("digest")?,
        canonical: row.try_get("canonical")?,
    })
}

pub(crate) fn page_size(limit: u32) -> Result<i64> {
    if !(1..=1000).contains(&limit) {
        return Err(Error::InvalidPageSize);
    }
    Ok(i64::from(limit))
}

impl Store {
    /// History is always addressed within an already authorized organization.
    pub async fn version(
        &self,
        org: &OrganizationId,
        name: &str,
        revision: i64,
    ) -> Result<Option<DeclarationVersion>> {
        sqlx::query("SELECT name,revision,digest,canonical FROM declaration_versions WHERE organization=$1 AND name=$2 AND revision=$3")
            .bind(org.as_str()).bind(name).bind(revision).fetch_optional(&self.pool).await?
            .map(version).transpose()
    }

    /// One MVCC snapshot covers both heads and the replay watermark.
    pub async fn snapshot(&self, org: &OrganizationId) -> Result<Snapshot> {
        let mut tx = self.read_transaction().await?;
        let watermark: Option<i64> = sqlx::query_scalar(
            "SELECT last_sequence FROM organization_streams WHERE organization=$1",
        )
        .bind(org.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let rows = sqlx::query("SELECT v.name,v.revision,v.digest,v.canonical FROM declaration_heads h JOIN declaration_versions v USING (organization,name,revision) WHERE h.organization=$1 ORDER BY h.name")
            .bind(org.as_str()).fetch_all(&mut *tx).await?;
        let declarations = rows.into_iter().map(version).collect::<Result<Vec<_>>>()?;
        tx.commit().await?;
        Ok(Snapshot {
            declarations,
            watermark: watermark.unwrap_or(0),
        })
    }

    pub async fn replay(&self, org: &OrganizationId, after: i64, limit: u32) -> Result<EventPage> {
        let limit = page_size(limit)?;
        if after < 0 {
            return Err(Error::InvalidCursor);
        }
        let mut tx = self.read_transaction().await?;
        let row = sqlx::query(
            "SELECT last_sequence,replay_floor FROM organization_streams WHERE organization=$1",
        )
        .bind(org.as_str())
        .fetch_optional(&mut *tx)
        .await?;
        let (watermark, floor): (i64, i64) = match row {
            Some(row) => (row.try_get("last_sequence")?, row.try_get("replay_floor")?),
            None => (0, 0),
        };
        if after < floor {
            return Err(Error::CursorExpired);
        }
        if after > watermark {
            return Err(Error::InvalidCursor);
        }
        let events = sqlx::query("SELECT sequence,kind,payload FROM events WHERE organization=$1 AND sequence>$2 ORDER BY sequence LIMIT $3")
            .bind(org.as_str()).bind(after).bind(limit).fetch_all(&mut *tx).await?
            .into_iter().map(event).collect::<Result<Vec<_>>>()?;
        let next_cursor = events.last().map_or(after, |e| e.sequence);
        tx.commit().await?;
        Ok(EventPage {
            events,
            next_cursor,
            watermark,
        })
    }

    /// Delivery is at least once: acknowledge only after the external sink accepts
    /// the event. Consumers deduplicate by (organization, sequence).
    pub async fn pending_outbox(&self, org: &OrganizationId, limit: u32) -> Result<Vec<Event>> {
        sqlx::query("SELECT e.sequence,e.kind,e.payload FROM outbox o JOIN events e USING (organization,sequence) WHERE o.organization=$1 AND NOT o.acknowledged ORDER BY o.sequence LIMIT $2")
            .bind(org.as_str()).bind(page_size(limit)?).fetch_all(&self.pool).await?
            .into_iter().map(event).collect()
    }
}
