//! The RSI event ledger: a bounded, per-tenant read cache of session events.
//!
//! The durable event stream already exists (JSONL locally, the `session_*`
//! projections in Postgres). What the diagnosis pass needs is different: a
//! *recent, in-memory window* per `(tenant, session)` that can be folded at any
//! moment without a database round trip inside a request.
//!
//! Two properties matter more than completeness:
//!
//! * **Tenant separation is structural.** The map key is
//!   `(principal, session)`, so a sweep started for one tenant can never read
//!   another tenant's events — even when two tenants happen to use the same
//!   session name.
//! * **Bounded.** Records per session and sessions per process are capped, and
//!   the least recently written session is evicted first. A ledger that grows
//!   without bound would be a slow memory leak in a long-lived server.
//!
//! Idempotence is by `seq`: a re-delivered batch is dropped instead of being
//! folded twice, because duplicate evidence would double-count a pathology.

use agent_core::seams::RsiConfig;
use agent_core::session::{SessionEvent, SessionPersistence, SessionRecord, SessionResult};
use async_trait::async_trait;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};

/// Sessions retained when the caller does not override the bound.
pub const DEFAULT_MAX_SESSIONS: usize = 512;

/// Health weight of one event: how much it contributes to "this session needs a
/// retrospective". Weights are small integers so the threshold stays readable.
fn unhealthy_weight(event: &SessionEvent) -> usize {
    match event {
        // A failed turn is the strongest signal there is.
        SessionEvent::TurnFailed { .. } => 2,
        SessionEvent::ItemDone { is_error: true, .. } => 1,
        SessionEvent::Approval { decision, .. } if decision == "deny" => 1,
        _ => 0,
    }
}

#[derive(Debug, Default)]
struct Buffer {
    records: VecDeque<SessionRecord>,
    /// Sequence numbers currently retained (duplicate detection).
    seen: BTreeSet<u64>,
    /// Highest sequence number already trimmed out of the window; anything at or
    /// below it was either folded or is older than the window.
    low_water: Option<u64>,
    last_at_ms: i64,
    unhealthy: usize,
}

/// Counters for observability (`GET /v1/rsi/status`), never content.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LedgerStats {
    pub sessions: usize,
    pub records: usize,
    pub unhealthy_sessions: usize,
}

/// Bounded per-tenant event window.
#[derive(Debug)]
pub struct RsiLedger {
    max_records: usize,
    max_sessions: usize,
    unhealthy_threshold: usize,
    buffers: Mutex<BTreeMap<(String, String), Buffer>>,
}

impl RsiLedger {
    /// Build from the deployment configuration.
    pub fn new(config: &RsiConfig) -> Self {
        Self::with_capacity(
            config.max_records,
            DEFAULT_MAX_SESSIONS,
            config.sweep_threshold,
        )
    }

    pub fn with_capacity(
        max_records: usize,
        max_sessions: usize,
        unhealthy_threshold: usize,
    ) -> Self {
        Self {
            max_records: max_records.max(1),
            max_sessions: max_sessions.max(1),
            unhealthy_threshold: unhealthy_threshold.max(1),
            buffers: Mutex::new(BTreeMap::new()),
        }
    }

    /// A [`SessionPersistence`] sink bound to one tenant.
    pub fn sink(self: &Arc<Self>, principal: impl Into<String>) -> LedgerSink {
        LedgerSink {
            ledger: self.clone(),
            principal: principal.into(),
        }
    }

    /// Append a batch for one tenant, dropping records already seen.
    pub fn append(
        &self,
        principal: &str,
        session: &str,
        records: &[SessionRecord],
    ) -> SessionResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        let mut buffers = self.lock();
        let key = (principal.to_string(), session.to_string());
        if !buffers.contains_key(&key) {
            if buffers.len() >= self.max_sessions {
                if let Some(oldest) = buffers
                    .iter()
                    .min_by_key(|(_, buffer)| buffer.last_at_ms)
                    .map(|(key, _)| key.clone())
                {
                    buffers.remove(&oldest);
                }
            }
            buffers.insert(key.clone(), Buffer::default());
        }
        let buffer = buffers.get_mut(&key).expect("buffer was just inserted");
        for record in records {
            // Exact duplicate (re-delivered batch): fold once.
            if buffer.seen.contains(&record.seq) {
                continue;
            }
            // Older than the retained window: it was trimmed after folding.
            if matches!(buffer.low_water, Some(water) if record.seq <= water) {
                continue;
            }
            buffer.seen.insert(record.seq);
            buffer.last_at_ms = record.at_ms;
            buffer.unhealthy += unhealthy_weight(&record.event);
            buffer.records.push_back(record.clone());
        }
        while buffer.records.len() > self.max_records {
            if let Some(evicted) = buffer.records.pop_front() {
                buffer.seen.remove(&evicted.seq);
                buffer.low_water = Some(match buffer.low_water {
                    Some(water) => water.max(evicted.seq),
                    None => evicted.seq,
                });
            }
        }
        Ok(())
    }

    /// The retained window for one session, oldest first.
    pub fn records(&self, principal: &str, session: &str) -> Vec<SessionRecord> {
        self.records_since(principal, session, 0)
    }

    /// Retained records with `seq >= from_seq`.
    pub fn records_since(
        &self,
        principal: &str,
        session: &str,
        from_seq: u64,
    ) -> Vec<SessionRecord> {
        let buffers = self.lock();
        let mut records: Vec<SessionRecord> = buffers
            .get(&(principal.to_string(), session.to_string()))
            .map(|buffer| {
                buffer
                    .records
                    .iter()
                    .filter(|record| record.seq >= from_seq)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        // Out-of-order delivery is possible; the fold requires sequence order.
        records.sort_by_key(|record| record.seq);
        records
    }

    /// Sessions of one tenant whose accumulated signal reached the threshold.
    ///
    /// Sorted by session id so a sweep is reproducible.
    pub fn due(&self, principal: &str) -> Vec<String> {
        let buffers = self.lock();
        buffers
            .iter()
            .filter(|((owner, _), buffer)| {
                owner == principal && buffer.unhealthy >= self.unhealthy_threshold
            })
            .map(|((_, session), _)| session.clone())
            .collect()
    }

    /// Tenants with at least one retained session, sorted.
    ///
    /// The periodic sweep uses this to find work; the API sweep is always scoped
    /// to the caller's own principal, so this is the only place a principal list
    /// is exposed.
    pub fn principals(&self) -> Vec<String> {
        let buffers = self.lock();
        let mut principals: Vec<String> = buffers
            .keys()
            .map(|(principal, _)| principal.clone())
            .collect();
        principals.dedup();
        principals
    }

    /// Clear a session's signal after a sweep (records are kept).
    pub fn reset(&self, principal: &str, session: &str) {
        let mut buffers = self.lock();
        if let Some(buffer) = buffers.get_mut(&(principal.to_string(), session.to_string())) {
            buffer.unhealthy = 0;
        }
    }

    /// Drop a session entirely (tenant deletion / tests).
    pub fn forget(&self, principal: &str, session: &str) {
        self.lock()
            .remove(&(principal.to_string(), session.to_string()));
    }

    pub fn stats(&self) -> LedgerStats {
        let buffers = self.lock();
        LedgerStats {
            sessions: buffers.len(),
            records: buffers.values().map(|buffer| buffer.records.len()).sum(),
            unhealthy_sessions: buffers
                .values()
                .filter(|buffer| buffer.unhealthy >= self.unhealthy_threshold)
                .count(),
        }
    }

    /// A poisoned lock means another thread panicked mid-fold. The data is still
    /// structurally valid (we only push/trim), so recovering beats failing a
    /// completed turn.
    fn lock(&self) -> MutexGuard<'_, BTreeMap<(String, String), Buffer>> {
        self.buffers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// [`SessionPersistence`] view of the ledger for exactly one tenant.
#[derive(Clone)]
pub struct LedgerSink {
    ledger: Arc<RsiLedger>,
    principal: String,
}

impl LedgerSink {
    pub fn principal(&self) -> &str {
        &self.principal
    }

    pub fn ledger(&self) -> &Arc<RsiLedger> {
        &self.ledger
    }
}

#[async_trait]
impl SessionPersistence for LedgerSink {
    async fn append(&self, session: &str, records: &[SessionRecord]) -> SessionResult<()> {
        self.ledger.append(&self.principal, session, records)
    }

    async fn read(&self, session: &str, from_seq: u64) -> SessionResult<Vec<SessionRecord>> {
        // A ledger that has not seen the session yet is empty, not an error: the
        // sink sits behind `ChainPersistence`, where a missing optional source
        // must not fail a read.
        Ok(self
            .ledger
            .records_since(&self.principal, session, from_seq))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(seq: u64, at_ms: i64, event: SessionEvent) -> SessionRecord {
        SessionRecord { seq, at_ms, event }
    }

    fn error_record(seq: u64, at_ms: i64) -> SessionRecord {
        record(
            seq,
            at_ms,
            SessionEvent::ItemDone {
                item_id: format!("i{seq}"),
                result: serde_json::json!({"ok": false}),
                is_error: true,
                latency_ms: Some(10),
            },
        )
    }

    #[test]
    fn records_are_windowed_deduplicated_and_sorted() {
        let ledger = RsiLedger::with_capacity(3, 8, 2);
        ledger
            .append(
                "t1",
                "s1",
                &[
                    record(2, 2, SessionEvent::TextDelta { text: "b".into() }),
                    record(0, 0, SessionEvent::TextDelta { text: "a".into() }),
                ],
            )
            .unwrap();
        let records = ledger.records("t1", "s1");
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].seq, 0, "out-of-order delivery is re-sorted");

        // Re-delivery of an already seen seq is dropped.
        ledger
            .append(
                "t1",
                "s1",
                &[
                    record(2, 2, SessionEvent::TextDelta { text: "b".into() }),
                    record(3, 3, SessionEvent::TextDelta { text: "c".into() }),
                    record(4, 4, SessionEvent::TextDelta { text: "d".into() }),
                ],
            )
            .unwrap();
        let records = ledger.records("t1", "s1");
        assert_eq!(records.len(), 3, "the window is capped at max_records");
        assert_eq!(records[2].seq, 4);

        // A record older than the trimmed window is dropped rather than
        // re-filling the buffer.
        ledger
            .append(
                "t1",
                "s1",
                &[record(1, 1, SessionEvent::TextDelta { text: "z".into() })],
            )
            .unwrap();
        assert_eq!(ledger.records("t1", "s1").len(), 3);

        let tail = ledger.records_since("t1", "s1", 4);
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 4);
        assert!(ledger.records("nobody", "s1").is_empty());
        // An empty batch is a no-op, not a new session.
        ledger.append("t1", "s1", &[]).unwrap();
        assert_eq!(ledger.stats().sessions, 1);
        assert_eq!(ledger.stats().records, 3);
    }

    #[test]
    fn tenants_are_separated_even_with_identical_session_names() {
        let ledger = RsiLedger::with_capacity(16, 8, 1);
        ledger
            .append("tenant-a", "s", &[error_record(0, 1)])
            .unwrap();
        assert_eq!(ledger.due("tenant-a"), vec!["s".to_string()]);
        assert!(ledger.due("tenant-b").is_empty());
        assert!(ledger.records("tenant-b", "s").is_empty());
    }

    #[test]
    fn principals_are_listed_once_in_order() {
        let ledger = RsiLedger::with_capacity(16, 8, 1);
        ledger
            .append("tenant-b", "s1", &[error_record(0, 1)])
            .unwrap();
        ledger
            .append("tenant-a", "s1", &[error_record(0, 1)])
            .unwrap();
        ledger
            .append("tenant-b", "s2", &[error_record(0, 2)])
            .unwrap();
        assert_eq!(
            ledger.principals(),
            vec!["tenant-a".to_string(), "tenant-b".to_string()]
        );
    }

    #[test]
    fn the_signal_counts_failures_errors_and_denials() {
        let ledger = RsiLedger::with_capacity(16, 8, 3);
        ledger
            .append(
                "t",
                "s",
                &[record(
                    0,
                    1,
                    SessionEvent::Approval {
                        id: "a".into(),
                        tool: "shell".into(),
                        decision: "deny".into(),
                        reason: None,
                    },
                )],
            )
            .unwrap();
        assert!(ledger.due("t").is_empty(), "1 < threshold 3");
        ledger
            .append(
                "t",
                "s",
                &[record(
                    1,
                    2,
                    SessionEvent::TurnFailed {
                        turn_id: "t1".into(),
                        message: "boom".into(),
                    },
                )],
            )
            .unwrap();
        assert_eq!(ledger.due("t"), vec!["s".to_string()], "1 + 2 == threshold");
        ledger.append("t", "s", &[error_record(2, 3)]).unwrap();
        assert_eq!(ledger.due("t"), vec!["s".to_string()]);
        assert_eq!(ledger.stats().unhealthy_sessions, 1);

        ledger.reset("t", "s");
        assert!(ledger.due("t").is_empty());
        assert_eq!(ledger.stats().unhealthy_sessions, 0);
        ledger.reset("t", "unknown");
        ledger.forget("t", "s");
        assert_eq!(ledger.stats().sessions, 0);
    }

    #[test]
    fn benign_events_carry_no_weight() {
        let ledger = RsiLedger::with_capacity(16, 8, 1);
        ledger
            .append(
                "t",
                "s",
                &[
                    record(
                        0,
                        1,
                        SessionEvent::Step {
                            phase: "model".into(),
                            label: None,
                        },
                    ),
                    record(
                        1,
                        2,
                        SessionEvent::Approval {
                            id: "a".into(),
                            tool: "shell".into(),
                            decision: "allow".into(),
                            reason: None,
                        },
                    ),
                    record(
                        2,
                        3,
                        SessionEvent::ItemDone {
                            item_id: "i".into(),
                            result: serde_json::json!(null),
                            is_error: false,
                            latency_ms: None,
                        },
                    ),
                ],
            )
            .unwrap();
        assert!(ledger.due("t").is_empty());
        assert_eq!(ledger.stats().records, 3);
    }

    #[test]
    fn the_session_cap_evicts_the_least_recently_written() {
        let ledger = RsiLedger::with_capacity(16, 2, 1);
        ledger.append("t", "old", &[error_record(0, 1)]).unwrap();
        ledger.append("t", "new", &[error_record(0, 2)]).unwrap();
        ledger.append("t", "newest", &[error_record(0, 3)]).unwrap();
        assert!(ledger.records("t", "old").is_empty());
        assert_eq!(ledger.stats().sessions, 2);
        assert_eq!(
            ledger.due("t"),
            vec!["new".to_string(), "newest".to_string()]
        );
    }

    #[test]
    fn zero_capacities_are_clamped_instead_of_disabling_the_ledger() {
        let ledger = RsiLedger::with_capacity(0, 0, 0);
        ledger
            .append(
                "t",
                "s",
                &[record(0, 1, SessionEvent::TextDone { text: "x".into() })],
            )
            .unwrap();
        assert_eq!(ledger.stats().records, 1);
        assert_eq!(ledger.stats().sessions, 1);
    }

    #[tokio::test]
    async fn the_sink_writes_and_reads_under_its_own_tenant() {
        let ledger = Arc::new(RsiLedger::with_capacity(16, 8, 1));
        let sink = ledger.sink("tenant-a");
        assert_eq!(sink.principal(), "tenant-a");
        assert_eq!(sink.ledger().stats().sessions, 0);
        sink.append("s", &[error_record(0, 1)]).await.unwrap();
        let read = sink.read("s", 0).await.unwrap();
        assert_eq!(read.len(), 1);
        assert!(sink.read("other", 0).await.unwrap().is_empty());
        assert_eq!(ledger.due("tenant-a"), vec!["s".to_string()]);
        assert!(ledger.due("tenant-b").is_empty());
    }

    #[test]
    fn configuration_drives_the_limits() {
        let config = RsiConfig {
            max_records: 4,
            sweep_threshold: 1,
            ..RsiConfig::default()
        };
        let ledger = RsiLedger::new(&config);
        ledger
            .append(
                "t",
                "s",
                &(0..6)
                    .map(|seq| {
                        record(
                            seq,
                            seq as i64,
                            SessionEvent::TextDelta { text: "x".into() },
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        assert_eq!(ledger.records("t", "s").len(), 4);
    }

    #[test]
    fn a_poisoned_lock_is_recovered_instead_of_failing_the_turn() {
        let ledger = Arc::new(RsiLedger::with_capacity(16, 8, 1));
        ledger.append("t", "s", &[error_record(0, 1)]).unwrap();
        let clone = ledger.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.buffers.lock().unwrap();
            panic!("poison the ledger");
        })
        .join();
        // The ledger must stay usable: a completed turn cannot be failed by a
        // diagnostic cache.
        ledger.append("t", "s", &[error_record(1, 2)]).unwrap();
        assert_eq!(ledger.records("t", "s").len(), 2);
    }
}
