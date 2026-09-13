//! Feedback binding — the "Observe" half of the Reef loop.
//!
//! Clients post [`Feedback`] (a numeric `score` plus `references` to the
//! [`Record`](crate::record::Record) receipt ids it concerns). Feedback is
//! stored locally (sled), never in Postgres/memo. [`is_eligible`] decides
//! whether enough signal exists to trigger an evolution pass.

use crate::error::ReefError;
use crate::record::RecordStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use uuid::Uuid;

/// A piece of feedback bound to one or more recorded turns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Feedback {
    pub id: String,
    /// Receipt ids of the [`Record`](crate::record::Record)s this feedback concerns.
    pub references: Vec<String>,
    /// Numeric quality signal. Convention: >0 good, <0 bad, 0 neutral.
    pub score: f64,
    /// Free-text or structured feedback.
    pub feedback: Option<String>,
    pub created_at: i64,
}

impl Feedback {
    pub fn new(references: Vec<String>, score: f64, feedback: Option<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            references,
            score,
            feedback,
            created_at: crate::record::now_ms(),
        }
    }
}

/// Persistence contract for [`Feedback`].
#[async_trait]
pub trait FeedbackStore: Send + Sync {
    /// Persist a feedback report.
    async fn report(&self, fb: Feedback) -> Result<(), ReefError>;
    /// Fetch feedback by id.
    async fn get(&self, id: &str) -> Result<Feedback, ReefError>;
    /// All feedback, newest last.
    async fn list(&self) -> Result<Vec<Feedback>, ReefError>;
    /// Feedback whose `references` include `record_id`.
    async fn list_by_record(&self, record_id: &str) -> Result<Vec<Feedback>, ReefError>;
}

/// sled-backed [`FeedbackStore`].
pub struct SledFeedbackStore {
    feedback: sled::Tree,
}

impl SledFeedbackStore {
    pub fn open(path: &std::path::Path) -> Result<Arc<Self>, ReefError> {
        let db = sled::open(path).map_err(|e| ReefError::Storage(e.to_string()))?;
        let feedback = db
            .open_tree("reef_feedback")
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { feedback }))
    }

    pub fn memory() -> Result<Arc<Self>, ReefError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        let feedback = db
            .open_tree("reef_feedback")
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { feedback }))
    }
}

#[async_trait]
impl FeedbackStore for SledFeedbackStore {
    async fn report(&self, fb: Feedback) -> Result<(), ReefError> {
        let value = serde_json::to_vec(&fb)?;
        self.feedback
            .insert(fb.id.as_bytes(), value)
            .map_err(|e| ReefError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get(&self, id: &str) -> Result<Feedback, ReefError> {
        let v = self
            .feedback
            .get(id.as_bytes())
            .map_err(|e| ReefError::Storage(e.to_string()))?
            .ok_or_else(|| ReefError::NotFound(id.to_string()))?;
        Ok(serde_json::from_slice(&v)?)
    }

    async fn list(&self) -> Result<Vec<Feedback>, ReefError> {
        let mut out = Vec::new();
        for item in self.feedback.iter() {
            let (_k, v) = item.map_err(|e| ReefError::Storage(e.to_string()))?;
            out.push(serde_json::from_slice(&v)?);
        }
        out.sort_by_key(|f: &Feedback| f.created_at);
        Ok(out)
    }

    async fn list_by_record(&self, record_id: &str) -> Result<Vec<Feedback>, ReefError> {
        let all = self.list().await?;
        Ok(all
            .into_iter()
            .filter(|f| f.references.iter().any(|r| r == record_id))
            .collect())
    }
}

/// Eligibility rule for triggering evolution.
///
/// A pass is eligible when there is enough signal to learn from: either the
/// number of feedback reports reaches `min_feedback`, or at least one report is
/// explicitly negative (`score < 0`). Empty feedback is never eligible.
pub fn is_eligible(feedbacks: &[Feedback], min_feedback: usize) -> bool {
    if feedbacks.is_empty() {
        return false;
    }
    if feedbacks.len() >= min_feedback {
        return true;
    }
    feedbacks.iter().any(|f| f.score < 0.0)
}

/// Convenience over a [`FeedbackStore`] + [`RecordStore`]: gather the records
/// referenced by all stored feedback (the eligible training set), or `NoEligible`
/// when there isn't enough signal yet.
pub async fn collect_eligible(
    feedback: &Arc<dyn FeedbackStore>,
    records: &Arc<dyn RecordStore>,
    min_feedback: usize,
) -> Result<Vec<crate::record::Record>, ReefError> {
    let all = feedback.list().await?;
    if !is_eligible(&all, min_feedback) {
        return Err(ReefError::NoEligible);
    }
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for fb in &all {
        for ref_id in &fb.references {
            if seen.insert(ref_id.clone()) {
                if let Ok(rec) = records.get(ref_id).await {
                    out.push(rec);
                }
            }
        }
    }
    if out.is_empty() {
        return Err(ReefError::NoEligible);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Record, SledRecordStore};

    #[tokio::test]
    async fn report_and_get_roundtrip() {
        let store = SledFeedbackStore::memory().unwrap();
        let fb = Feedback::new(vec!["r1".into(), "r2".into()], -1.0, Some("wrong".into()));
        store.report(fb.clone()).await.unwrap();
        let got = store.get(&fb.id).await.unwrap();
        assert_eq!(got.score, -1.0);
        assert_eq!(got.references, vec!["r1", "r2"]);
    }

    #[tokio::test]
    async fn get_missing_is_not_found() {
        let store = SledFeedbackStore::memory().unwrap();
        assert!(matches!(
            store.get("nope").await.unwrap_err(),
            ReefError::NotFound(_)
        ));
    }

    #[tokio::test]
    async fn list_by_record_filters_references() {
        let store = SledFeedbackStore::memory().unwrap();
        store
            .report(Feedback::new(vec!["r1".into()], 1.0, None))
            .await
            .unwrap();
        store
            .report(Feedback::new(vec!["r2".into()], 1.0, None))
            .await
            .unwrap();
        assert_eq!(store.list_by_record("r1").await.unwrap().len(), 1);
        assert_eq!(store.list_by_record("rX").await.unwrap().len(), 0);
    }

    #[test]
    fn eligibility_boundaries() {
        assert!(!is_eligible(&[], 1));
        // One neutral report below threshold: not eligible (no negative).
        assert!(!is_eligible(
            &[Feedback::new(vec!["r".into()], 0.0, None)],
            2
        ));
        // Negative report makes it eligible immediately.
        assert!(is_eligible(
            &[Feedback::new(vec!["r".into()], -1.0, None)],
            2
        ));
        // Reaching the count threshold also makes it eligible.
        assert!(is_eligible(
            &[
                Feedback::new(vec!["r1".into()], 1.0, None),
                Feedback::new(vec!["r2".into()], 1.0, None)
            ],
            2
        ));
    }

    #[tokio::test]
    async fn collect_eligible_returns_referenced_records() {
        let recs: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
        let fb: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
        let rec = Record::new("a", "s", None, "sys", "in", "out", "stub");
        recs.record_turn(rec.clone()).await.unwrap();
        // Below threshold and no negative score -> not eligible.
        fb.report(Feedback::new(vec![rec.id.clone()], 0.5, None))
            .await
            .unwrap();
        assert!(matches!(
            collect_eligible(&fb, &recs, 2).await,
            Err(ReefError::NoEligible)
        ));
        // Now a negative report makes it eligible and the referenced record is returned.
        fb.report(Feedback::new(vec![rec.id.clone()], -1.0, None))
            .await
            .unwrap();
        let eligible = collect_eligible(&fb, &recs, 2).await.unwrap();
        assert_eq!(eligible.len(), 1);
        assert_eq!(eligible[0].id, rec.id);
    }

    #[tokio::test]
    async fn collect_eligible_skips_dangling_references() {
        let recs: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
        let fb: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
        fb.report(Feedback::new(vec!["ghost".into()], -1.0, None))
            .await
            .unwrap();
        assert!(matches!(
            collect_eligible(&fb, &recs, 2).await,
            Err(ReefError::NoEligible)
        ));
    }
}
