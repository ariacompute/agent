//! Schedule seam (`ctx.schedule`).
//!
//! Deliberately no cron parser: an entry is either "every N seconds" or "at an
//! epoch instant", which covers the reminder use case without pulling a date
//! library into the kernel. The store, the single-flight ticker and tenant
//! scoping live in the provider (ADR-0017).

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One scheduled reminder / job.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    #[serde(default)]
    pub id: String,
    /// Tenancy scope. Every query filters on it (AGENTS.md rule 7).
    pub owner: String,
    pub name: String,
    /// Repeat every N seconds (`None` = one-shot).
    #[serde(default)]
    pub every_secs: Option<u64>,
    /// One-shot instant in epoch milliseconds.
    #[serde(default)]
    pub at_ms: Option<i64>,
    /// Free-form payload handed back to the run loop.
    #[serde(default)]
    pub payload: Value,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Provider-computed next fire time (epoch ms).
    #[serde(default)]
    pub next_run_ms: Option<i64>,
}

fn default_true() -> bool {
    true
}

impl ScheduleEntry {
    pub fn every(owner: impl Into<String>, name: impl Into<String>, every_secs: u64) -> Self {
        Self {
            id: String::new(),
            owner: owner.into(),
            name: name.into(),
            every_secs: Some(every_secs),
            at_ms: None,
            payload: Value::Null,
            enabled: true,
            next_run_ms: None,
        }
    }

    pub fn at(owner: impl Into<String>, name: impl Into<String>, at_ms: i64) -> Self {
        Self {
            id: String::new(),
            owner: owner.into(),
            name: name.into(),
            every_secs: None,
            at_ms: Some(at_ms),
            payload: Value::Null,
            enabled: true,
            next_run_ms: None,
        }
    }

    /// Exactly one trigger shape, positive interval, non-empty owner/name.
    pub fn validate(&self) -> SeamResult<()> {
        if self.owner.trim().is_empty() {
            return Err(SeamError::Invalid(
                "schedule owner must not be empty".into(),
            ));
        }
        if self.name.trim().is_empty() {
            return Err(SeamError::Invalid("schedule name must not be empty".into()));
        }
        match (self.every_secs, self.at_ms) {
            (Some(secs), None) if secs > 0 => Ok(()),
            (None, Some(at)) if at > 0 => Ok(()),
            (Some(_), None) => Err(SeamError::Invalid("every_secs must be > 0".into())),
            (None, Some(_)) => Err(SeamError::Invalid("at_ms must be > 0".into())),
            _ => Err(SeamError::Invalid(
                "exactly one of every_secs / at_ms is required".into(),
            )),
        }
    }

    /// Next fire time strictly after `now_ms`, if the entry still fires.
    pub fn next_after(&self, now_ms: i64) -> Option<i64> {
        if !self.enabled {
            return None;
        }
        if let Some(secs) = self.every_secs {
            let step = (secs as i64).saturating_mul(1000);
            if step <= 0 {
                return None;
            }
            return Some(now_ms - now_ms.rem_euclid(step) + step);
        }
        self.at_ms.filter(|at| *at > now_ms)
    }
}

#[async_trait]
pub trait ScheduleSeam: Send + Sync {
    async fn upsert(&self, entry: ScheduleEntry) -> SeamResult<ScheduleEntry>;
    async fn list(&self, owner: &str) -> SeamResult<Vec<ScheduleEntry>>;
    async fn remove(&self, owner: &str, id: &str) -> SeamResult<bool>;
}

/// Fail-closed default: scheduling is not configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSchedule;

#[async_trait]
impl ScheduleSeam for NoSchedule {
    async fn upsert(&self, _entry: ScheduleEntry) -> SeamResult<ScheduleEntry> {
        Err(SeamError::Unavailable(
            "no schedule provider is configured for this deployment".into(),
        ))
    }

    async fn list(&self, _owner: &str) -> SeamResult<Vec<ScheduleEntry>> {
        Ok(Vec::new())
    }

    async fn remove(&self, _owner: &str, _id: &str) -> SeamResult<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_entry_validates_and_computes_next() {
        let e = ScheduleEntry::every("t1", "ping", 60);
        assert!(e.validate().is_ok());
        assert_eq!(e.next_after(0), Some(60_000));
        assert_eq!(e.next_after(59_000), Some(60_000));
        assert_eq!(e.next_after(60_000), Some(120_000));
    }

    #[test]
    fn one_shot_stops_after_its_instant() {
        let e = ScheduleEntry::at("t1", "once", 1_000);
        assert!(e.validate().is_ok());
        assert_eq!(e.next_after(500), Some(1_000));
        assert_eq!(e.next_after(1_000), None);
        assert_eq!(e.next_after(2_000), None);
    }

    #[test]
    fn disabled_entries_never_fire() {
        let mut e = ScheduleEntry::every("t1", "ping", 10);
        e.enabled = false;
        assert_eq!(e.next_after(0), None);
    }

    #[test]
    fn validation_rejects_bad_shapes() {
        assert!(matches!(
            ScheduleEntry::every("", "n", 1).validate(),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            ScheduleEntry::every("t", " ", 1).validate(),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            ScheduleEntry::every("t", "n", 0).validate(),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            ScheduleEntry::at("t", "n", 0).validate(),
            Err(SeamError::Invalid(_))
        ));
        let mut both = ScheduleEntry::every("t", "n", 1);
        both.at_ms = Some(5);
        assert!(matches!(both.validate(), Err(SeamError::Invalid(_))));
        let mut neither = ScheduleEntry::every("t", "n", 1);
        neither.every_secs = None;
        assert!(matches!(neither.validate(), Err(SeamError::Invalid(_))));
    }

    #[test]
    fn serde_defaults_and_non_firing_schedules() {
        let entry: ScheduleEntry = serde_json::from_value(serde_json::json!({
            "id": "x",
            "owner": "t",
            "name": "n",
            "every_secs": 10,
            "at_ms": null,
            "payload": null,
            "next_run_ms": null
        }))
        .unwrap();
        assert!(entry.enabled);
        let mut zero = ScheduleEntry::every("t", "n", 1);
        zero.every_secs = Some(0);
        assert_eq!(zero.next_after(100), None);
    }

    #[tokio::test]
    async fn no_schedule_is_fail_closed() {
        let s = NoSchedule;
        assert!(matches!(
            s.upsert(ScheduleEntry::every("t", "n", 1)).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(s.list("t").await.unwrap().is_empty());
        assert!(!s.remove("t", "id").await.unwrap());
    }
}
