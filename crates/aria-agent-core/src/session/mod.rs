//! Session event stream: the single source of truth for a conversation.
//!
//! dsh owns an **append-only** `Session` that emits a persistent event stream
//! (`ctx.sessions`) and lets backends persist it (`ctx.sessionPersistence`) and
//! query it (`ctx.sessionQuery`). The Rust port keeps one vocabulary
//! ([`SessionEvent`]) shared by the JSONL (local) and Postgres (cloud) providers,
//! and derives read models as [`projection`]s instead of storing them twice
//! (ADR-0015).

pub mod compaction;
pub mod jsonl;
pub mod projection;
pub mod query;
pub mod title;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use compaction::{CompactDecision, CompactReason, CompactionPolicy};
pub use jsonl::JsonlPersistence;
pub use projection::{ItemView, SessionProjection, TodoItem, TurnStatus, TurnView};
pub use query::{query, search_text, SessionFilter};
pub use title::{
    first_prompt_title, FallbackOnly, SessionTitleProvider, SOURCE_FIRST_PROMPT, SOURCE_PROVIDER,
};

/// A single append-only session event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    TurnStarted {
        turn_id: String,
        input: String,
    },
    Step {
        phase: String,
        #[serde(default)]
        label: Option<String>,
    },
    ItemAdded {
        item_id: String,
        kind: String,
        payload: Value,
    },
    ItemDone {
        item_id: String,
        #[serde(default)]
        result: Value,
        #[serde(default)]
        is_error: bool,
    },
    TextDelta {
        text: String,
    },
    TextDone {
        text: String,
    },
    Approval {
        id: String,
        tool: String,
        decision: String,
        #[serde(default)]
        reason: Option<String>,
    },
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        #[serde(default)]
        model: String,
    },
    Compacted {
        fragment_id: String,
        removed: usize,
        reason: String,
    },
    TitleSet {
        title: String,
        source: String,
    },
    TurnCompleted {
        turn_id: String,
        output: String,
    },
    TurnFailed {
        turn_id: String,
        message: String,
    },
}

impl SessionEvent {
    /// Stable event kind (query filter / wire name).
    pub fn kind(&self) -> &'static str {
        match self {
            SessionEvent::TurnStarted { .. } => "turn_started",
            SessionEvent::Step { .. } => "step",
            SessionEvent::ItemAdded { .. } => "item_added",
            SessionEvent::ItemDone { .. } => "item_done",
            SessionEvent::TextDelta { .. } => "text_delta",
            SessionEvent::TextDone { .. } => "text_done",
            SessionEvent::Approval { .. } => "approval",
            SessionEvent::Usage { .. } => "usage",
            SessionEvent::Compacted { .. } => "compacted",
            SessionEvent::TitleSet { .. } => "title_set",
            SessionEvent::TurnCompleted { .. } => "turn_completed",
            SessionEvent::TurnFailed { .. } => "turn_failed",
        }
    }

    /// Terminal events close a turn.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            SessionEvent::TurnCompleted { .. } | SessionEvent::TurnFailed { .. }
        )
    }

    /// Turn this event belongs to, when it carries one.
    pub fn turn_id(&self) -> Option<&str> {
        match self {
            SessionEvent::TurnStarted { turn_id, .. }
            | SessionEvent::TurnCompleted { turn_id, .. }
            | SessionEvent::TurnFailed { turn_id, .. } => Some(turn_id),
            _ => None,
        }
    }

    /// Plain text carried by the event (used by full-text search).
    pub fn text(&self) -> String {
        match self {
            SessionEvent::TurnStarted { input, .. } => input.clone(),
            SessionEvent::TextDelta { text } | SessionEvent::TextDone { text } => text.clone(),
            SessionEvent::TurnCompleted { output, .. } => output.clone(),
            SessionEvent::TurnFailed { message, .. } => message.clone(),
            SessionEvent::Approval {
                tool,
                reason,
                decision,
                ..
            } => format!(
                "{tool} {decision} {}",
                reason.as_deref().unwrap_or_default()
            ),
            SessionEvent::Step { phase, label } => {
                format!("{phase} {}", label.as_deref().unwrap_or_default())
            }
            SessionEvent::ItemAdded { kind, payload, .. } => format!("{kind} {payload}"),
            SessionEvent::ItemDone { result, .. } => result.to_string(),
            SessionEvent::Compacted { reason, .. } => reason.clone(),
            SessionEvent::TitleSet { title, .. } => title.clone(),
            SessionEvent::Usage { model, .. } => model.clone(),
        }
    }
}

/// A sequenced event as persisted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub seq: u64,
    pub at_ms: i64,
    pub event: SessionEvent,
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum SessionError {
    #[error("session io error: {0}")]
    Io(String),
    #[error("session serialization error: {0}")]
    Serde(String),
    #[error("session not found: {0}")]
    NotFound(String),
}

pub type SessionResult<T> = Result<T, SessionError>;

/// Persistence backing for [`SessionRecord`]s.
#[async_trait]
pub trait SessionPersistence: Send + Sync {
    /// Append records in order. Implementations must be idempotent per `seq`.
    async fn append(&self, session: &str, records: &[SessionRecord]) -> SessionResult<()>;
    /// Read records with `seq >= from_seq`, oldest first.
    async fn read(&self, session: &str, from_seq: u64) -> SessionResult<Vec<SessionRecord>>;
}

/// Buffered recorder: assigns sequence numbers, batches writes to the end of a
/// turn (dsh flushes at turn boundaries, not per token).
pub struct SessionRecorder {
    session: String,
    next_seq: u64,
    pending: Vec<SessionRecord>,
    persistence: Option<std::sync::Arc<dyn SessionPersistence>>,
}

impl SessionRecorder {
    pub fn new(session: impl Into<String>) -> Self {
        Self {
            session: session.into(),
            next_seq: 0,
            pending: Vec::new(),
            persistence: None,
        }
    }

    pub fn with_persistence(
        session: impl Into<String>,
        persistence: std::sync::Arc<dyn SessionPersistence>,
    ) -> Self {
        Self {
            session: session.into(),
            next_seq: 0,
            pending: Vec::new(),
            persistence: Some(persistence),
        }
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// Record one event, assigning the next sequence number.
    pub fn push(&mut self, event: SessionEvent) -> &SessionRecord {
        let record = SessionRecord {
            seq: self.next_seq,
            at_ms: crate::context::now_ms(),
            event,
        };
        self.next_seq += 1;
        self.pending.push(record);
        self.pending.last().expect("just pushed")
    }

    pub fn pending(&self) -> &[SessionRecord] {
        &self.pending
    }

    /// Flush buffered records to the backend (no-op without persistence).
    pub async fn flush(&mut self) -> SessionResult<()> {
        let Some(backend) = self.persistence.clone() else {
            return Ok(());
        };
        if self.pending.is_empty() {
            return Ok(());
        }
        let batch: Vec<SessionRecord> = std::mem::take(&mut self.pending);
        if let Err(e) = backend.append(&self.session, &batch).await {
            // Keep the unwritten records so the next flush retries them instead of
            // silently dropping the audit trail.
            let mut restored = batch;
            restored.append(&mut self.pending);
            self.pending = restored;
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn event_kinds_terminals_and_turn_ids() {
        let started = SessionEvent::TurnStarted {
            turn_id: "t1".into(),
            input: "hi".into(),
        };
        assert_eq!(started.kind(), "turn_started");
        assert_eq!(started.turn_id(), Some("t1"));
        assert!(!started.is_terminal());

        let done = SessionEvent::TurnCompleted {
            turn_id: "t1".into(),
            output: "ok".into(),
        };
        assert!(done.is_terminal());
        assert_eq!(done.turn_id(), Some("t1"));

        let failed = SessionEvent::TurnFailed {
            turn_id: "t1".into(),
            message: "boom".into(),
        };
        assert!(failed.is_terminal());
        assert_eq!(failed.text(), "boom");

        let step = SessionEvent::Step {
            phase: "model".into(),
            label: None,
        };
        assert_eq!(step.turn_id(), None);
        assert_eq!(step.text(), "model ");
    }

    #[test]
    fn every_event_serializes_with_a_type_tag() {
        let events = vec![
            SessionEvent::TurnStarted {
                turn_id: "t".into(),
                input: "i".into(),
            },
            SessionEvent::Step {
                phase: "p".into(),
                label: Some("l".into()),
            },
            SessionEvent::ItemAdded {
                item_id: "i1".into(),
                kind: "tool_call".into(),
                payload: json!({"name": "shell"}),
            },
            SessionEvent::ItemDone {
                item_id: "i1".into(),
                result: json!({"exit": 0}),
                is_error: false,
            },
            SessionEvent::TextDelta { text: "a".into() },
            SessionEvent::TextDone { text: "a".into() },
            SessionEvent::Approval {
                id: "a1".into(),
                tool: "shell".into(),
                decision: "allow".into(),
                reason: None,
            },
            SessionEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                model: "m".into(),
            },
            SessionEvent::Compacted {
                fragment_id: "f".into(),
                removed: 3,
                reason: "pressure".into(),
            },
            SessionEvent::TitleSet {
                title: "T".into(),
                source: "first_prompt".into(),
            },
            SessionEvent::TurnCompleted {
                turn_id: "t".into(),
                output: "o".into(),
            },
            SessionEvent::TurnFailed {
                turn_id: "t".into(),
                message: "m".into(),
            },
        ];
        for ev in events {
            let v = serde_json::to_value(&ev).unwrap();
            assert_eq!(v["type"], ev.kind());
            let back: SessionEvent = serde_json::from_value(v).unwrap();
            assert_eq!(back, ev);
        }
    }

    #[test]
    fn text_extraction_covers_every_variant() {
        let cases = vec![
            SessionEvent::TextDelta { text: "d".into() },
            SessionEvent::TextDone { text: "d".into() },
            SessionEvent::ItemAdded {
                item_id: "i".into(),
                kind: "tool_call".into(),
                payload: json!("x"),
            },
            SessionEvent::ItemDone {
                item_id: "i".into(),
                result: json!("y"),
                is_error: true,
            },
            SessionEvent::Approval {
                id: "a".into(),
                tool: "shell".into(),
                decision: "deny".into(),
                reason: Some("nope".into()),
            },
            SessionEvent::Compacted {
                fragment_id: "f".into(),
                removed: 1,
                reason: "budget".into(),
            },
            SessionEvent::TitleSet {
                title: "Title".into(),
                source: "llm".into(),
            },
            SessionEvent::Usage {
                input_tokens: 0,
                output_tokens: 0,
                model: "gpt".into(),
            },
        ];
        for ev in cases {
            assert!(!ev.text().is_empty(), "{ev:?}");
        }
    }

    #[tokio::test]
    async fn recorder_assigns_sequence_and_flushes() {
        let dir = tempfile::tempdir().unwrap();
        let backend: std::sync::Arc<dyn SessionPersistence> =
            std::sync::Arc::new(JsonlPersistence::new(dir.path()));
        let mut rec = SessionRecorder::with_persistence("s1", backend.clone());
        let seq0 = rec
            .push(SessionEvent::TurnStarted {
                turn_id: "t".into(),
                input: "hi".into(),
            })
            .seq;
        let seq1 = rec
            .push(SessionEvent::TurnCompleted {
                turn_id: "t".into(),
                output: "ok".into(),
            })
            .seq;
        assert_eq!((seq0, seq1), (0, 1));
        assert_eq!(rec.pending().len(), 2);
        rec.flush().await.unwrap();
        assert!(rec.pending().is_empty());
        let read = backend.read("s1", 0).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].event.kind(), "turn_completed");
        // Flushing an empty buffer is a no-op.
        rec.flush().await.unwrap();
        assert_eq!(rec.session(), "s1");
    }

    #[tokio::test]
    async fn recorder_without_backend_only_buffers() {
        let mut rec = SessionRecorder::new("s");
        rec.push(SessionEvent::TextDelta { text: "x".into() });
        rec.flush().await.unwrap();
        assert_eq!(rec.pending().len(), 1);
    }

    /// Backend that always refuses, standing in for a full disk.
    struct FailingBackend;

    #[async_trait::async_trait]
    impl SessionPersistence for FailingBackend {
        async fn append(&self, _session: &str, _records: &[SessionRecord]) -> SessionResult<()> {
            Err(SessionError::Io("disk full".into()))
        }

        async fn read(&self, _session: &str, _from_seq: u64) -> SessionResult<Vec<SessionRecord>> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn failed_flush_keeps_the_records_for_a_retry() {
        let backend: std::sync::Arc<dyn SessionPersistence> = std::sync::Arc::new(FailingBackend);
        let mut rec = SessionRecorder::with_persistence("s1", backend);
        rec.push(SessionEvent::TextDelta { text: "x".into() });
        assert!(rec.flush().await.is_err());
        assert_eq!(rec.pending().len(), 1, "unwritten records are retained");
        // A second attempt sees the same batch (order preserved) and fails again.
        assert!(rec.flush().await.is_err());
        assert_eq!(rec.pending().len(), 1);
        rec.push(SessionEvent::TextDelta { text: "y".into() });
        assert_eq!(rec.pending().len(), 2);
    }
}
