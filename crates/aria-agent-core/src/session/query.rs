//! Session queries: filtering + cursor paging + full-text.
//!
//! dsh's `ctx.sessionQuery` backend exposes exact reads, ordering and cursor
//! generations, while rendering stays with the consumer. Here the pure functions
//! are the contract; a backend may push them down to SQL without changing them.

use super::{SessionEvent, SessionRecord};
use serde::{Deserialize, Serialize};

/// Default page size for a query.
pub const DEFAULT_LIMIT: usize = 100;
/// Hard cap so a client cannot ask for an unbounded page.
pub const MAX_LIMIT: usize = 1000;

/// Filter for [`query`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFilter {
    /// Only records belonging to this turn.
    #[serde(default)]
    pub turn_id: Option<String>,
    /// Only this event kind (`text_delta`, `item_added`, …).
    #[serde(default)]
    pub kind: Option<String>,
    /// Cursor: records with `seq >= from_seq`.
    #[serde(default)]
    pub from_seq: u64,
    /// Page size (`0` → [`DEFAULT_LIMIT`], capped at [`MAX_LIMIT`]).
    #[serde(default)]
    pub limit: usize,
}

impl SessionFilter {
    pub fn kind(kind: impl Into<String>) -> Self {
        Self {
            kind: Some(kind.into()),
            ..Self::default()
        }
    }

    pub fn turn(turn_id: impl Into<String>) -> Self {
        Self {
            turn_id: Some(turn_id.into()),
            ..Self::default()
        }
    }

    pub fn effective_limit(&self) -> usize {
        match self.limit {
            0 => DEFAULT_LIMIT,
            n => n.min(MAX_LIMIT),
        }
    }
}

/// Apply a filter to an in-memory log.
pub fn query(records: &[SessionRecord], filter: &SessionFilter) -> Vec<SessionRecord> {
    records
        .iter()
        .filter(|r| r.seq >= filter.from_seq)
        .filter(|r| match &filter.kind {
            Some(kind) => r.event.kind() == kind,
            None => true,
        })
        .filter(|r| match &filter.turn_id {
            Some(turn) => record_turn(r).as_deref() == Some(turn.as_str()),
            None => true,
        })
        .take(filter.effective_limit())
        .cloned()
        .collect()
}

/// A record "belongs to" a turn when it names that turn, or when it is any event
/// inside a turn that started with it. The second half needs the start index, so
/// this helper only handles the explicit case; [`query`] walks turns via
/// [`annotate_turns`].
fn record_turn(record: &SessionRecord) -> Option<String> {
    record.event.turn_id().map(str::to_string)
}

/// Attach the enclosing turn id to every record (streamed events carry none).
pub fn annotate_turns(records: &[SessionRecord]) -> Vec<(Option<String>, SessionRecord)> {
    let mut current: Option<String> = None;
    records
        .iter()
        .map(|r| {
            if let SessionEvent::TurnStarted { turn_id, .. } = &r.event {
                current = Some(turn_id.clone());
            }
            let owner = current.clone();
            if r.event.is_terminal() {
                current = None;
            }
            (owner, r.clone())
        })
        .collect()
}

/// Query with turn ownership resolved through [`annotate_turns`].
pub fn query_with_turns(records: &[SessionRecord], filter: &SessionFilter) -> Vec<SessionRecord> {
    let annotated = annotate_turns(records);
    annotated
        .into_iter()
        .filter(|(_, r)| r.seq >= filter.from_seq)
        .filter(|(_, r)| match &filter.kind {
            Some(kind) => r.event.kind() == kind,
            None => true,
        })
        .filter(|(turn, _)| match &filter.turn_id {
            Some(want) => turn.as_deref() == Some(want.as_str()),
            None => true,
        })
        .take(filter.effective_limit())
        .map(|(_, r)| r)
        .collect()
}

/// Case-insensitive full-text scan over the event payload text.
pub fn search_text(records: &[SessionRecord], needle: &str) -> Vec<SessionRecord> {
    let needle = needle.trim().to_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    records
        .iter()
        .filter(|r| r.event.text().to_lowercase().contains(&needle))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn log() -> Vec<SessionRecord> {
        vec![
            SessionRecord {
                seq: 0,
                at_ms: 0,
                event: SessionEvent::TurnStarted {
                    turn_id: "t1".into(),
                    input: "List the files".into(),
                },
            },
            SessionRecord {
                seq: 1,
                at_ms: 0,
                event: SessionEvent::Step {
                    phase: "model".into(),
                    label: None,
                },
            },
            SessionRecord {
                seq: 2,
                at_ms: 0,
                event: SessionEvent::TextDelta {
                    text: "Hello".into(),
                },
            },
            SessionRecord {
                seq: 3,
                at_ms: 0,
                event: SessionEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    output: "Hello".into(),
                },
            },
            SessionRecord {
                seq: 4,
                at_ms: 0,
                event: SessionEvent::TurnStarted {
                    turn_id: "t2".into(),
                    input: "Now summarise".into(),
                },
            },
            SessionRecord {
                seq: 5,
                at_ms: 0,
                event: SessionEvent::TurnCompleted {
                    turn_id: "t2".into(),
                    output: "Done".into(),
                },
            },
        ]
    }

    #[test]
    fn filters_by_kind_and_cursor() {
        let all = query(&log(), &SessionFilter::default());
        assert_eq!(all.len(), 6);
        let deltas = query(&log(), &SessionFilter::kind("text_delta"));
        assert_eq!(deltas.len(), 1);
        assert_eq!(deltas[0].seq, 2);
        let tail = query(
            &log(),
            &SessionFilter {
                from_seq: 4,
                ..Default::default()
            },
        );
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].seq, 4);
    }

    #[test]
    fn paging_respects_limit_and_cap() {
        let f = SessionFilter {
            limit: 2,
            ..Default::default()
        };
        assert_eq!(query(&log(), &f).len(), 2);
        assert_eq!(
            SessionFilter {
                limit: 0,
                ..Default::default()
            }
            .effective_limit(),
            DEFAULT_LIMIT
        );
        assert_eq!(
            SessionFilter {
                limit: usize::MAX,
                ..Default::default()
            }
            .effective_limit(),
            MAX_LIMIT
        );
    }

    #[test]
    fn turn_filter_uses_enclosing_turn() {
        let only_t1 = query_with_turns(&log(), &SessionFilter::turn("t1"));
        assert_eq!(only_t1.len(), 4);
        assert_eq!(only_t1.first().unwrap().seq, 0);
        assert_eq!(only_t1.last().unwrap().seq, 3);
        let only_t2 = query_with_turns(&log(), &SessionFilter::turn("t2"));
        assert_eq!(only_t2.len(), 2);
        assert!(query_with_turns(&log(), &SessionFilter::turn("nope")).is_empty());
    }

    #[test]
    fn annotate_turns_resets_after_terminal_event() {
        let annotated = annotate_turns(&log());
        assert_eq!(annotated[0].0.as_deref(), Some("t1"));
        assert_eq!(annotated[2].0.as_deref(), Some("t1"));
        assert_eq!(annotated[4].0.as_deref(), Some("t2"));
    }

    #[test]
    fn full_text_search_is_case_insensitive() {
        let hits = search_text(&log(), "hello");
        assert_eq!(hits.len(), 2);
        assert_eq!(search_text(&log(), "  ").len(), 0);
        assert_eq!(search_text(&log(), "zzz").len(), 0);
        assert_eq!(search_text(&log(), "SUMMARISE").len(), 1);
    }
}
