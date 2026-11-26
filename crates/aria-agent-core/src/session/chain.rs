//! Fan-out [`SessionPersistence`]: one event stream, several sinks.
//!
//! A deployment needs the ledger written to more than one place at once — the
//! audit JSONL, the RSI ledger, a title projection — without teaching every
//! caller about each sink. [`ChainPersistence`] is that composition, and it has
//! one deliberate asymmetry:
//!
//! * **The first backend is authoritative.** If it fails, the whole append fails
//!   so [`SessionRecorder`](super::SessionRecorder) keeps the records and retries
//!   them on the next turn (a silently lost audit trail is worse than a retry).
//! * **Every later backend is best effort.** One optional sink being down must
//!   not fail an otherwise completed turn; it is logged and the remaining sinks
//!   still receive the batch.
//!
//! Reads return the first backend that actually has records, so a write-only
//! projection (the cloud's title projection errors on read by design) can sit in
//! a chain without poisoning it.

use super::{SessionError, SessionPersistence, SessionRecord, SessionResult};
use async_trait::async_trait;
use std::sync::Arc;

/// Ordered fan-out of session persistence sinks.
#[derive(Clone)]
pub struct ChainPersistence {
    backends: Vec<Arc<dyn SessionPersistence>>,
}

impl std::fmt::Debug for ChainPersistence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainPersistence")
            .field("backends", &self.backends.len())
            .finish()
    }
}

impl ChainPersistence {
    pub fn new(backends: Vec<Arc<dyn SessionPersistence>>) -> Self {
        Self { backends }
    }

    /// An empty chain: appends succeed and reads report nothing.
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Builder form used by the assembly points.
    pub fn with(mut self, backend: Arc<dyn SessionPersistence>) -> Self {
        self.backends.push(backend);
        self
    }

    pub fn len(&self) -> usize {
        self.backends.len()
    }

    pub fn is_empty(&self) -> bool {
        self.backends.is_empty()
    }
}

#[async_trait]
impl SessionPersistence for ChainPersistence {
    async fn append(&self, session: &str, records: &[SessionRecord]) -> SessionResult<()> {
        let mut iter = self.backends.iter();
        let Some(primary) = iter.next() else {
            return Ok(());
        };
        primary.append(session, records).await?;
        for (index, backend) in iter.enumerate() {
            if let Err(e) = backend.append(session, records).await {
                // Best effort by contract: the authoritative sink already has the
                // batch, so an optional sink failing is a warning, not a failure.
                tracing::warn!(
                    session = %session,
                    sink = index + 1,
                    error = %e,
                    "session sink rejected a batch that the primary sink accepted"
                );
            }
        }
        Ok(())
    }

    async fn read(&self, session: &str, from_seq: u64) -> SessionResult<Vec<SessionRecord>> {
        let mut first_error: Option<SessionError> = None;
        for backend in &self.backends {
            match backend.read(session, from_seq).await {
                Ok(records) if !records.is_empty() => return Ok(records),
                Ok(_) => {}
                // "This sink never saw the session" is not a chain failure.
                Err(SessionError::NotFound(_)) => {}
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }
        // A chain reports an empty read as empty: across several sinks,
        // "nobody has records" cannot be distinguished from "not created".
        match first_error {
            Some(e) => Err(e),
            None => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionEvent;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MemSink {
        records: Mutex<Vec<SessionRecord>>,
        fail_append: bool,
        fail_read: bool,
    }

    impl MemSink {
        fn failing_append() -> Self {
            Self {
                fail_append: true,
                ..Self::default()
            }
        }

        fn failing_read() -> Self {
            Self {
                fail_read: true,
                ..Self::default()
            }
        }

        fn seeded(records: Vec<SessionRecord>) -> Self {
            Self {
                records: Mutex::new(records),
                ..Self::default()
            }
        }

        fn len(&self) -> usize {
            self.records.lock().unwrap().len()
        }
    }

    #[async_trait]
    impl SessionPersistence for MemSink {
        async fn append(&self, _session: &str, records: &[SessionRecord]) -> SessionResult<()> {
            if self.fail_append {
                return Err(SessionError::Io("sink down".into()));
            }
            self.records.lock().unwrap().extend_from_slice(records);
            Ok(())
        }

        async fn read(&self, _session: &str, from_seq: u64) -> SessionResult<Vec<SessionRecord>> {
            if self.fail_read {
                return Err(SessionError::NotFound("write-only".into()));
            }
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .filter(|r| r.seq >= from_seq)
                .cloned()
                .collect())
        }
    }

    fn record(seq: u64) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: 0,
            event: SessionEvent::TextDelta {
                text: format!("t{seq}"),
            },
        }
    }

    #[tokio::test]
    async fn an_empty_chain_is_a_no_op() {
        let chain = ChainPersistence::empty();
        assert!(chain.is_empty());
        assert_eq!(chain.len(), 0);
        chain.append("s", &[record(0)]).await.unwrap();
        assert!(chain.read("s", 0).await.unwrap().is_empty());
        assert!(format!("{chain:?}").contains("ChainPersistence"));
    }

    #[tokio::test]
    async fn every_sink_receives_the_batch_in_order() {
        let a = Arc::new(MemSink::default());
        let b = Arc::new(MemSink::default());
        let chain = ChainPersistence::new(Vec::new())
            .with(a.clone())
            .with(b.clone());
        assert_eq!(chain.len(), 2);
        chain.append("s", &[record(0), record(1)]).await.unwrap();
        assert_eq!(a.len(), 2);
        assert_eq!(b.len(), 2);
        let read = chain.read("s", 0).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].seq, 1);
    }

    #[tokio::test]
    async fn a_failing_primary_fails_the_whole_append() {
        let primary = Arc::new(MemSink::failing_append());
        let secondary = Arc::new(MemSink::default());
        let chain = ChainPersistence::new(vec![primary, secondary.clone()]);
        assert!(matches!(
            chain.append("s", &[record(0)]).await,
            Err(SessionError::Io(_))
        ));
        assert_eq!(
            secondary.len(),
            0,
            "secondaries are not written after a primary failure"
        );
    }

    #[tokio::test]
    async fn a_failing_secondary_is_best_effort() {
        let primary = Arc::new(MemSink::default());
        let secondary = Arc::new(MemSink::failing_append());
        let chain = ChainPersistence::new(vec![primary.clone(), secondary]);
        chain.append("s", &[record(0)]).await.unwrap();
        assert_eq!(primary.len(), 1);
    }

    #[tokio::test]
    async fn reads_skip_write_only_and_failing_sinks() {
        let write_only = Arc::new(MemSink::failing_read());
        let data = Arc::new(MemSink::seeded(vec![record(3)]));
        let chain = ChainPersistence::new(vec![write_only, data]);
        let read = chain.read("s", 0).await.unwrap();
        assert_eq!(read.len(), 1);
        assert_eq!(read[0].seq, 3);
        assert!(chain.read("s", 4).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn reads_surface_the_first_real_error_when_nothing_has_data() {
        struct Broken;
        #[async_trait]
        impl SessionPersistence for Broken {
            async fn append(&self, _s: &str, _r: &[SessionRecord]) -> SessionResult<()> {
                Ok(())
            }
            async fn read(&self, _s: &str, _f: u64) -> SessionResult<Vec<SessionRecord>> {
                Err(SessionError::Serde("corrupt".into()))
            }
        }
        let broken = Arc::new(Broken);
        // The failing sink still accepts writes; only its read is broken.
        broken.append("s", &[record(0)]).await.unwrap();
        let chain = ChainPersistence::new(vec![broken, Arc::new(MemSink::default())]);
        assert!(matches!(
            chain.read("s", 0).await,
            Err(SessionError::Serde(_))
        ));
    }
}
