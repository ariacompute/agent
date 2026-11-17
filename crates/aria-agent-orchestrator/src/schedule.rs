//! Scheduled jobs: a tiny store plus a single-flight ticker.
//!
//! No cron parser: an entry is either `every_secs` or `at_ms` (see
//! [`ScheduleEntry`]). The ticker claims a due entry by **advancing its
//! `next_run_ms` before running it**, so a second instance (or a restarted one)
//! can never fire the same instant twice; a `try_lock` on the ticker keeps a
//! single process from overlapping ticks (ADR-0017).
//!
//! The store is owner-scoped on every read, mirroring the cloud tables.

use agent_core::seams::{ScheduleEntry, ScheduleSeam, SeamError, SeamResult};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// Executes one due entry.
#[async_trait]
pub trait ScheduleRunner: Send + Sync {
    async fn run(&self, entry: &ScheduleEntry) -> SeamResult<()>;
}

/// Current wall-clock time in epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// First fire time for an entry that was just created.
///
/// A one-shot entry keeps its instant even when it is already past: a reminder
/// that was created offline should still fire on the next tick instead of
/// silently vanishing.
fn initial_next_run(entry: &ScheduleEntry, now: i64) -> Option<i64> {
    if !entry.enabled {
        return None;
    }
    match (entry.every_secs, entry.at_ms) {
        (Some(_), None) => entry.next_after(now),
        (None, Some(at)) => Some(at),
        _ => None,
    }
}

/// In-memory schedule store + ticker.
#[derive(Default)]
pub struct MemorySchedule {
    entries: Mutex<BTreeMap<String, ScheduleEntry>>,
    seq: AtomicU64,
    ticker: tokio::sync::Mutex<()>,
}

impl MemorySchedule {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> SeamResult<std::sync::MutexGuard<'_, BTreeMap<String, ScheduleEntry>>> {
        self.entries
            .lock()
            .map_err(|e| SeamError::Io(format!("schedule store poisoned: {e}")))
    }

    /// Entries that would fire at `now` (read-only; does not claim them).
    pub fn due(&self, now: i64) -> SeamResult<Vec<ScheduleEntry>> {
        let guard = self.lock()?;
        Ok(guard
            .values()
            .filter(|e| e.enabled && e.next_run_ms.map(|n| n <= now).unwrap_or(false))
            .cloned()
            .collect())
    }

    /// Claim every due entry: advance its `next_run_ms` (or disable a one-shot)
    /// *before* it runs, so a crash cannot re-fire the same instant.
    fn claim_due(&self, now: i64) -> SeamResult<Vec<ScheduleEntry>> {
        let mut guard = self.lock()?;
        let mut claimed = Vec::new();
        for entry in guard.values_mut() {
            if !entry.enabled {
                continue;
            }
            let Some(next) = entry.next_run_ms else {
                continue;
            };
            if next > now {
                continue;
            }
            let following = entry.next_after(now);
            entry.next_run_ms = following;
            if following.is_none() {
                entry.enabled = false;
            }
            claimed.push(entry.clone());
        }
        Ok(claimed)
    }

    /// Run every due entry at most once.
    ///
    /// Returns the ids that were fired. A runner failure is logged and does not
    /// abort the remaining entries (one broken job must not stall the ticker).
    pub async fn tick(&self, now: i64, runner: &dyn ScheduleRunner) -> SeamResult<Vec<String>> {
        let _guard = match self.ticker.try_lock() {
            Ok(guard) => guard,
            // Another tick owns the current instant.
            Err(_) => return Ok(Vec::new()),
        };
        let claimed = self.claim_due(now)?;
        let mut fired = Vec::with_capacity(claimed.len());
        for entry in claimed {
            if let Err(e) = runner.run(&entry).await {
                let job = entry.id.as_str();
                let name = entry.name.as_str();
                let error = e.to_string();
                tracing::warn!(job, name, error, "scheduled job failed");
            }
            fired.push(entry.id);
        }
        Ok(fired)
    }

    /// Number of stored entries (diagnostics/tests).
    pub fn len(&self) -> SeamResult<usize> {
        Ok(self.lock()?.len())
    }

    pub fn is_empty(&self) -> SeamResult<bool> {
        Ok(self.lock()?.is_empty())
    }
}

#[async_trait]
impl ScheduleSeam for MemorySchedule {
    async fn upsert(&self, mut entry: ScheduleEntry) -> SeamResult<ScheduleEntry> {
        entry.validate()?;
        let now = now_ms();
        if entry.id.trim().is_empty() {
            entry.id = format!("job-{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1);
        }
        entry.next_run_ms = initial_next_run(&entry, now);
        let mut guard = self.lock()?;
        // An entry id is only unique per owner: the same id from another tenant
        // must not be overwritten.
        if let Some(existing) = guard.get(&entry.id) {
            if existing.owner != entry.owner {
                return Err(SeamError::Denied(format!(
                    "schedule `{}` belongs to another tenant",
                    entry.id
                )));
            }
        }
        guard.insert(entry.id.clone(), entry.clone());
        Ok(entry)
    }

    async fn list(&self, owner: &str) -> SeamResult<Vec<ScheduleEntry>> {
        Ok(self
            .lock()?
            .values()
            .filter(|e| e.owner == owner)
            .cloned()
            .collect())
    }

    async fn remove(&self, owner: &str, id: &str) -> SeamResult<bool> {
        let mut guard = self.lock()?;
        match guard.get(id) {
            Some(entry) if entry.owner == owner => {
                guard.remove(id);
                Ok(true)
            }
            Some(_) => Err(SeamError::Denied(format!(
                "schedule `{id}` belongs to another tenant"
            ))),
            None => Ok(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    struct Counting {
        hits: AtomicUsize,
        seen: Mutex<Vec<String>>,
        fail: bool,
        delay: std::time::Duration,
    }

    impl Counting {
        fn new(fail: bool) -> Self {
            Self {
                hits: AtomicUsize::new(0),
                seen: Mutex::new(Vec::new()),
                fail,
                delay: std::time::Duration::ZERO,
            }
        }

        fn slow(mut self) -> Self {
            self.delay = std::time::Duration::from_millis(40);
            self
        }
    }

    #[async_trait]
    impl ScheduleRunner for Counting {
        async fn run(&self, entry: &ScheduleEntry) -> SeamResult<()> {
            self.hits.fetch_add(1, Ordering::SeqCst);
            self.seen.lock().unwrap().push(entry.id.clone());
            if !self.delay.is_zero() {
                tokio::time::sleep(self.delay).await;
            }
            if self.fail {
                return Err(SeamError::Io("runner exploded".into()));
            }
            Ok(())
        }
    }

    #[test]
    fn initial_next_run_covers_every_trigger_shape() {
        let now = 1_000;
        let mut bare = ScheduleEntry::every("t", "n", 1);
        bare.every_secs = None;
        assert_eq!(initial_next_run(&bare, now), None);
        let mut at = ScheduleEntry::at("t", "n", 42);
        assert_eq!(initial_next_run(&at, now), Some(42));
        at.enabled = false;
        assert_eq!(initial_next_run(&at, now), None);
    }

    #[test]
    fn a_poisoned_store_is_reported_instead_of_panicking() {
        let store = MemorySchedule::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = store.entries.lock().unwrap();
            panic!("poison the schedule store");
        }));
        assert!(result.is_err());
        assert!(matches!(store.due(0), Err(SeamError::Io(_))));
    }

    #[tokio::test]
    async fn unscheduled_entries_are_skipped_by_claim_due() {
        let store = MemorySchedule::new();
        let mut bare = ScheduleEntry::every("t", "n", 1);
        bare.every_secs = None;
        bare.at_ms = None;
        store.entries.lock().unwrap().insert("job-1".into(), bare);
        let runner = Counting::new(false);
        assert!(store.tick(1_000, &runner).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn upsert_assigns_ids_and_scopes_by_owner() {
        let store = MemorySchedule::new();
        let mut entry = ScheduleEntry::every("t1", "ping", 60);
        entry.payload = json!({"prompt": "check"});
        let stored = store.upsert(entry).await.unwrap();
        assert_eq!(stored.id, "job-1");
        assert!(stored.next_run_ms.is_some());
        assert_eq!(stored.payload["prompt"], "check");

        store
            .upsert(ScheduleEntry::every("t2", "other", 60))
            .await
            .unwrap();
        assert_eq!(store.list("t1").await.unwrap().len(), 1);
        assert_eq!(store.list("t2").await.unwrap().len(), 1);
        assert!(store.list("t3").await.unwrap().is_empty());

        assert!(matches!(
            store.upsert(ScheduleEntry::every("t1", " ", 5)).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(store.remove("t1", "job-1").await.unwrap());
        assert!(!store.remove("t1", "job-1").await.unwrap());
        assert_eq!(store.len().unwrap(), 1);
        assert!(!store.is_empty().unwrap());
    }

    #[tokio::test]
    async fn another_tenant_cannot_touch_an_id() {
        let store = MemorySchedule::new();
        let stored = store
            .upsert(ScheduleEntry::every("t1", "ping", 60))
            .await
            .unwrap();
        let mut hijack = ScheduleEntry::every("t2", "ping", 60);
        hijack.id = stored.id.clone();
        assert!(matches!(
            store.upsert(hijack).await,
            Err(SeamError::Denied(_))
        ));
        assert!(matches!(
            store.remove("t2", &stored.id).await,
            Err(SeamError::Denied(_))
        ));
    }

    #[tokio::test]
    async fn tick_fires_due_entries_once_and_advances() {
        let store = MemorySchedule::new();
        let stored = store
            .upsert(ScheduleEntry::every("t1", "ping", 1))
            .await
            .unwrap();
        let next = stored.next_run_ms.unwrap();
        let runner = Counting::new(false);

        assert!(store.due(next - 1).unwrap().is_empty());
        assert!(store.tick(next - 1, &runner).await.unwrap().is_empty());
        assert_eq!(runner.hits.load(Ordering::SeqCst), 0);

        let fired = store.tick(next, &runner).await.unwrap();
        assert_eq!(fired, vec![stored.id.clone()]);
        assert_eq!(runner.hits.load(Ordering::SeqCst), 1);
        // The same instant cannot fire twice.
        assert!(store.tick(next, &runner).await.unwrap().is_empty());
        assert_eq!(runner.hits.load(Ordering::SeqCst), 1);
        assert_eq!(store.due(next + 1000).unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_one_shot_fires_once_then_stops() {
        let store = MemorySchedule::new();
        let stored = store
            .upsert(ScheduleEntry::at("t1", "remind", 5_000))
            .await
            .unwrap();
        assert_eq!(stored.next_run_ms, Some(5_000));
        let runner = Counting::new(false);
        assert_eq!(store.tick(5_000, &runner).await.unwrap().len(), 1);
        assert!(store.tick(10_000, &runner).await.unwrap().is_empty());
        let after = store.list("t1").await.unwrap();
        assert!(!after[0].enabled);
        assert!(after[0].next_run_ms.is_none());
        assert_eq!(runner.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_past_one_shot_still_fires() {
        let store = MemorySchedule::new();
        store
            .upsert(ScheduleEntry::at("t1", "late", 1))
            .await
            .unwrap();
        let runner = Counting::new(false);
        assert_eq!(store.tick(now_ms(), &runner).await.unwrap().len(), 1);
        assert_eq!(runner.hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disabled_entries_never_fire() {
        let store = MemorySchedule::new();
        let mut entry = ScheduleEntry::every("t1", "paused", 1);
        entry.enabled = false;
        let stored = store.upsert(entry).await.unwrap();
        assert!(stored.next_run_ms.is_none());
        let runner = Counting::new(false);
        assert!(store
            .tick(now_ms() + 10_000, &runner)
            .await
            .unwrap()
            .is_empty());
        assert_eq!(runner.hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_failing_runner_does_not_stop_the_tick() {
        let store = MemorySchedule::new();
        store
            .upsert(ScheduleEntry::every("t1", "a", 1))
            .await
            .unwrap();
        let mut second = ScheduleEntry::every("t1", "b", 1);
        second.id = "b".into();
        store.upsert(second).await.unwrap();
        let runner = Counting::new(true);
        let fired = store.tick(now_ms() + 5_000, &runner).await.unwrap();
        assert_eq!(fired.len(), 2);
        assert_eq!(runner.hits.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn concurrent_ticks_are_single_flight() {
        let store = Arc::new(MemorySchedule::new());
        store
            .upsert(ScheduleEntry::every("t1", "job", 1))
            .await
            .unwrap();
        let runner = Arc::new(Counting::new(false).slow());
        let at = now_ms() + 5_000;
        let (a, b) = tokio::join!(
            store.tick(at, runner.as_ref()),
            store.tick(at, runner.as_ref())
        );
        assert_eq!(
            a.unwrap().len() + b.unwrap().len(),
            1,
            "only one tick fires"
        );
        assert_eq!(runner.hits.load(Ordering::SeqCst), 1);
        assert_eq!(runner.seen.lock().unwrap().len(), 1);
    }
}
