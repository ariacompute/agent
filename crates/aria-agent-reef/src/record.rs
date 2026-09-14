//! Turn recording — the "Serve" half of the Reef loop.
//!
//! Every agent turn is persisted as a [`Record`] (the receipt later used to
//! bind feedback). Records are interaction logs for *learning* and are kept in
//! a local/embedded sled store — deliberately **not** in Postgres (which only
//! holds `agents`/`runs` metadata) and **not** in memo (which only holds
//! conversational/long-term context). See `docs/adr/0005-reef-self-improvement.md`.

use crate::error::ReefError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use uuid::Uuid;

/// One recorded agent turn (the interaction receipt).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub agent_id: String,
    pub session: String,
    /// Optional scenario label (mirrors Reef's `x-reef-scenario` header).
    pub scenario: Option<String>,
    /// The system prompt actually served for this turn (so evolution can
    /// attribute outcomes to a specific harness version).
    pub system: String,
    pub input: String,
    pub output: String,
    pub model: String,
    /// Unix epoch milliseconds.
    pub created_at: i64,
}

impl Record {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        agent_id: &str,
        session: &str,
        scenario: Option<String>,
        system: &str,
        input: &str,
        output: &str,
        model: &str,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            agent_id: agent_id.to_string(),
            session: session.to_string(),
            scenario,
            system: system.to_string(),
            input: input.to_string(),
            output: output.to_string(),
            model: model.to_string(),
            created_at: now_ms(),
        }
    }
}

/// Persistence contract for [`Record`]s.
#[async_trait]
pub trait RecordStore: Send + Sync {
    /// Persist a recorded turn.
    async fn record_turn(&self, rec: Record) -> Result<(), ReefError>;
    /// Fetch a record by its receipt id.
    async fn get(&self, id: &str) -> Result<Record, ReefError>;
    /// All records for an agent, newest last.
    async fn list_by_agent(&self, agent_id: &str) -> Result<Vec<Record>, ReefError>;
    /// All records for a session, newest last.
    async fn list_by_session(&self, session: &str) -> Result<Vec<Record>, ReefError>;
}

/// sled-backed [`RecordStore`]. Self-contained, local/embedded.
pub struct SledRecordStore {
    records: sled::Tree,
}

impl SledRecordStore {
    pub fn open(path: &Path) -> Result<Arc<Self>, ReefError> {
        let db = sled::open(path).map_err(|e| ReefError::Storage(e.to_string()))?;
        let records = db
            .open_tree("reef_records")
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { records }))
    }

    /// In-memory variant (tests, SDK defaults).
    pub fn memory() -> Result<Arc<Self>, ReefError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        let records = db
            .open_tree("reef_records")
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { records }))
    }
}

#[async_trait]
impl RecordStore for SledRecordStore {
    async fn record_turn(&self, rec: Record) -> Result<(), ReefError> {
        let key = rec.id.as_bytes().to_vec();
        let value = serde_json::to_vec(&rec)?;
        self.records
            .insert(key, value)
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Record, ReefError> {
        let v = self
            .records
            .get(id.as_bytes())
            .map_err(|e| ReefError::Storage(e.to_string()))?
            .ok_or_else(|| ReefError::NotFound(id.to_string()))?;
        Ok(serde_json::from_slice(&v)?)
    }

    async fn list_by_agent(&self, agent_id: &str) -> Result<Vec<Record>, ReefError> {
        collect(&self.records, |r: &Record| r.agent_id == agent_id)
    }

    async fn list_by_session(&self, session: &str) -> Result<Vec<Record>, ReefError> {
        collect(&self.records, |r: &Record| r.session == session)
    }
}

fn collect(tree: &sled::Tree, pred: impl Fn(&Record) -> bool) -> Result<Vec<Record>, ReefError> {
    let mut out = Vec::new();
    for item in tree.iter() {
        let (_k, v) = item.map_err(|e| ReefError::Storage(e.to_string()))?;
        let rec: Record = serde_json::from_slice(&v)?;
        if pred(&rec) {
            out.push(rec);
        }
    }
    // Keep deterministic ordering by creation time (oldest first).
    out.sort_by_key(|r| r.created_at);
    Ok(out)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn record_and_get_roundtrip() {
        let store = SledRecordStore::memory().unwrap();
        let rec = Record::new(
            "a1",
            "s1",
            Some("chat".into()),
            "sys",
            "hi",
            "hello",
            "stub",
        );
        store.record_turn(rec.clone()).await.unwrap();
        let got = store.get(&rec.id).await.unwrap();
        assert_eq!(got.input, "hi");
        assert_eq!(got.output, "hello");
        assert_eq!(got.scenario.as_deref(), Some("chat"));
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let store = SledRecordStore::memory().unwrap();
        let err = store.get("nope").await.unwrap_err();
        assert!(matches!(err, ReefError::NotFound(_)));
    }

    #[tokio::test]
    async fn list_filters_by_agent_and_session() {
        let store = SledRecordStore::memory().unwrap();
        store
            .record_turn(Record::new("a1", "s1", None, "sys", "x", "y", "stub"))
            .await
            .unwrap();
        store
            .record_turn(Record::new("a1", "s2", None, "sys", "p", "q", "stub"))
            .await
            .unwrap();
        store
            .record_turn(Record::new("a2", "s1", None, "sys", "u", "v", "stub"))
            .await
            .unwrap();
        assert_eq!(store.list_by_agent("a1").await.unwrap().len(), 2);
        assert_eq!(store.list_by_session("s1").await.unwrap().len(), 2);
        assert_eq!(store.list_by_agent("aX").await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn list_is_ordered_oldest_first() {
        let store = SledRecordStore::memory().unwrap();
        let r1 = Record::new("a", "s", None, "sys", "1", "1", "stub");
        let r2 = Record::new("a", "s", None, "sys", "2", "2", "stub");
        store.record_turn(r2).await.unwrap();
        store.record_turn(r1).await.unwrap();
        let all = store.list_by_agent("a").await.unwrap();
        assert_eq!(all.len(), 2);
        assert!(all[0].created_at <= all[1].created_at);
    }
}
