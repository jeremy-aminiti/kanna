//! A durable, bounded mailbox. Reading does not acknowledge a batch; the
//! subscriber advances its position only by acknowledging that batch's id.
use super::Db;
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventSubscription {
    pub id: String,
    pub task_id: String,
    pub run_id: String,
    pub stage: Option<String>,
    pub branch: Option<String>,
    pub query: Value,
    pub delivery: String,
    pub revision: i64,
    pub cursor: Option<String>,
    pub pending: Option<Value>,
    pub batch_id: i64,
    pub wake_state: String,
    pub error: Option<String>,
    pub active: bool,
}

fn decode(row: &rusqlite::Row<'_>) -> rusqlite::Result<EventSubscription> {
    let encoded: String = row.get(0)?;
    serde_json::from_str(&encoded).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

impl Db {
    pub(crate) fn insert_event_subscription(
        &self,
        subscription: &EventSubscription,
    ) -> rusqlite::Result<()> {
        let record = serde_json::to_string(subscription)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        self.conn.execute(
            "INSERT INTO event_subscription (id, task_id, revision, record) VALUES (?, ?, ?, ?)",
            params![
                subscription.id,
                subscription.task_id,
                subscription.revision,
                record
            ],
        )?;
        Ok(())
    }

    pub(crate) fn event_subscription(
        &self,
        id: &str,
    ) -> rusqlite::Result<Option<EventSubscription>> {
        self.conn
            .query_row(
                "SELECT record FROM event_subscription WHERE id = ?",
                [id],
                decode,
            )
            .optional()
    }

    pub(crate) fn event_subscriptions(&self) -> rusqlite::Result<Vec<EventSubscription>> {
        self.conn
            .prepare("SELECT record FROM event_subscription ORDER BY id")?
            .query_map([], decode)?
            .collect()
    }

    /// CAS protects acknowledgements from late network/delivery responses.
    pub(crate) fn save_event_subscription(
        &self,
        subscription: &mut EventSubscription,
    ) -> rusqlite::Result<bool> {
        let previous = subscription.revision;
        subscription.revision += 1;
        let record = serde_json::to_string(subscription)
            .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
        let saved = self.conn.execute(
            "UPDATE event_subscription SET record = ?, revision = ? WHERE id = ? AND revision = ?",
            params![record, subscription.revision, subscription.id, previous],
        )? == 1;
        if !saved {
            subscription.revision = previous;
        }
        Ok(saved)
    }
}
