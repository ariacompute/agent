//! Read models derived from the event stream.
//!
//! dsh registers "projection units" (`ctx.sessionProjections`) that fold events
//! into state (turns, items, title, todos, plan). Projections are pure functions
//! of the event log, so replaying the log rebuilds them exactly — nothing is
//! stored twice.

use super::{SessionEvent, SessionRecord};
use crate::seams::TokenUsage;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TodoItem {
    pub id: String,
    pub content: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ItemView {
    pub id: String,
    pub kind: String,
    #[serde(default)]
    pub payload: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnView {
    pub id: String,
    pub input: String,
    #[serde(default)]
    pub output: Option<String>,
    pub status: TurnStatus,
    #[serde(default)]
    pub steps: Vec<String>,
    #[serde(default)]
    pub items: Vec<ItemView>,
}

/// Folded session state.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SessionProjection {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub title_source: Option<String>,
    #[serde(default)]
    pub turns: Vec<TurnView>,
    #[serde(default)]
    pub usage: TokenUsage,
    #[serde(default)]
    pub todos: Vec<TodoItem>,
    #[serde(default)]
    pub plan: Option<String>,
    #[serde(default)]
    pub compactions: usize,
    /// Text streamed for the in-flight turn (not persisted).
    #[serde(default)]
    pub partial_text: String,
    #[serde(default)]
    pub approvals: Vec<ApprovalView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalView {
    pub id: String,
    pub tool: String,
    pub decision: String,
    #[serde(default)]
    pub reason: Option<String>,
}

impl SessionProjection {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from a full log.
    pub fn replay(records: &[SessionRecord]) -> Self {
        let mut projection = Self::new();
        for record in records {
            projection.apply(record);
        }
        projection
    }

    /// Fold one event.
    pub fn apply(&mut self, record: &SessionRecord) {
        match &record.event {
            SessionEvent::TurnStarted { turn_id, input } => {
                self.partial_text.clear();
                self.turns.push(TurnView {
                    id: turn_id.clone(),
                    input: input.clone(),
                    output: None,
                    status: TurnStatus::Running,
                    steps: Vec::new(),
                    items: Vec::new(),
                });
            }
            SessionEvent::Step { phase, .. } => {
                if let Some(turn) = self.current_turn_mut() {
                    turn.steps.push(phase.clone());
                }
            }
            SessionEvent::ItemAdded {
                item_id,
                kind,
                payload,
            } => {
                self.apply_item_side_effects(kind, payload);
                if let Some(turn) = self.current_turn_mut() {
                    turn.items.push(ItemView {
                        id: item_id.clone(),
                        kind: kind.clone(),
                        payload: payload.clone(),
                        result: None,
                        is_error: false,
                    });
                }
            }
            SessionEvent::ItemDone {
                item_id,
                result,
                is_error,
            } => {
                if let Some(item) = self
                    .turns
                    .iter_mut()
                    .flat_map(|t| t.items.iter_mut())
                    .find(|i| i.id == *item_id)
                {
                    item.result = Some(result.clone());
                    item.is_error = *is_error;
                }
            }
            SessionEvent::TextDelta { text } => self.partial_text.push_str(text),
            SessionEvent::TextDone { text } => {
                self.partial_text = text.clone();
            }
            SessionEvent::Approval {
                id,
                tool,
                decision,
                reason,
            } => self.approvals.push(ApprovalView {
                id: id.clone(),
                tool: tool.clone(),
                decision: decision.clone(),
                reason: reason.clone(),
            }),
            SessionEvent::Usage {
                input_tokens,
                output_tokens,
                model,
            } => self.usage.merge(&TokenUsage::new(
                *input_tokens,
                *output_tokens,
                model.clone(),
            )),
            SessionEvent::Compacted { .. } => self.compactions += 1,
            SessionEvent::TitleSet { title, source } => {
                self.title = Some(title.clone());
                self.title_source = Some(source.clone());
            }
            SessionEvent::TurnCompleted { turn_id, output } => {
                self.close_turn(turn_id, Some(output.clone()), TurnStatus::Completed)
            }
            SessionEvent::TurnFailed { turn_id, message } => {
                self.close_turn(turn_id, Some(message.clone()), TurnStatus::Failed)
            }
        }
    }

    fn apply_item_side_effects(&mut self, kind: &str, payload: &Value) {
        match kind {
            "todo" => {
                if let Some(list) = payload.get("todos").and_then(Value::as_array) {
                    self.todos = list
                        .iter()
                        .filter_map(|t| {
                            Some(TodoItem {
                                id: t.get("id")?.as_str()?.to_string(),
                                content: t.get("content")?.as_str()?.to_string(),
                                status: t
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending")
                                    .to_string(),
                            })
                        })
                        .collect();
                }
            }
            "plan" => {
                self.plan = payload
                    .get("plan")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            _ => {}
        }
    }

    fn close_turn(&mut self, turn_id: &str, output: Option<String>, status: TurnStatus) {
        if let Some(turn) = self.turns.iter_mut().find(|t| t.id == turn_id) {
            turn.output = output;
            turn.status = status;
        }
        self.partial_text.clear();
    }

    fn current_turn_mut(&mut self) -> Option<&mut TurnView> {
        self.turns.last_mut()
    }

    /// Turn by id (read helper used by the HTTP layer).
    pub fn turn(&self, turn_id: &str) -> Option<&TurnView> {
        self.turns.iter().find(|t| t.id == turn_id)
    }

    /// Flattened item list across turns.
    pub fn items(&self) -> Vec<&ItemView> {
        self.turns.iter().flat_map(|t| t.items.iter()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(seq: u64, event: SessionEvent) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: 0,
            event,
        }
    }

    fn full_log() -> Vec<SessionRecord> {
        vec![
            rec(
                0,
                SessionEvent::TurnStarted {
                    turn_id: "t1".into(),
                    input: "list files".into(),
                },
            ),
            rec(
                1,
                SessionEvent::Step {
                    phase: "model".into(),
                    label: None,
                },
            ),
            rec(
                2,
                SessionEvent::ItemAdded {
                    item_id: "i1".into(),
                    kind: "tool_call".into(),
                    payload: json!({"name": "shell"}),
                },
            ),
            rec(
                3,
                SessionEvent::ItemDone {
                    item_id: "i1".into(),
                    result: json!({"exit": 0}),
                    is_error: false,
                },
            ),
            rec(4, SessionEvent::TextDelta { text: "he".into() }),
            rec(5, SessionEvent::TextDelta { text: "llo".into() }),
            rec(
                6,
                SessionEvent::Approval {
                    id: "a1".into(),
                    tool: "shell".into(),
                    decision: "allow".into(),
                    reason: None,
                },
            ),
            rec(
                7,
                SessionEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    model: "gpt".into(),
                },
            ),
            rec(
                8,
                SessionEvent::Compacted {
                    fragment_id: "f1".into(),
                    removed: 2,
                    reason: "pressure".into(),
                },
            ),
            rec(
                9,
                SessionEvent::TitleSet {
                    title: "File listing".into(),
                    source: "first_prompt".into(),
                },
            ),
            rec(
                10,
                SessionEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    output: "hello".into(),
                },
            ),
        ]
    }

    #[test]
    fn replay_folds_every_event_family() {
        let p = SessionProjection::replay(&full_log());
        assert_eq!(p.turns.len(), 1);
        let turn = &p.turns[0];
        assert_eq!(turn.id, "t1");
        assert_eq!(turn.status, TurnStatus::Completed);
        assert_eq!(turn.output.as_deref(), Some("hello"));
        assert_eq!(turn.steps, vec!["model"]);
        assert_eq!(turn.items.len(), 1);
        assert_eq!(turn.items[0].result, Some(json!({"exit": 0})));
        assert_eq!(p.usage.total(), 14);
        assert_eq!(p.compactions, 1);
        assert_eq!(p.title.as_deref(), Some("File listing"));
        assert_eq!(p.title_source.as_deref(), Some("first_prompt"));
        assert_eq!(p.approvals.len(), 1);
        assert_eq!(p.approvals[0].decision, "allow");
        assert!(p.partial_text.is_empty());
        assert_eq!(p.items().len(), 1);
        assert_eq!(p.turn("t1").unwrap().id, "t1");
        assert!(p.turn("nope").is_none());
    }

    #[test]
    fn todos_and_plan_are_projected_from_items() {
        let log = vec![
            rec(
                0,
                SessionEvent::TurnStarted {
                    turn_id: "t".into(),
                    input: "plan".into(),
                },
            ),
            rec(
                1,
                SessionEvent::ItemAdded {
                    item_id: "i".into(),
                    kind: "todo".into(),
                    payload: json!({"todos": [
                        {"id": "1", "content": "write code", "status": "pending"},
                        {"id": "2", "content": "test"}
                    ]}),
                },
            ),
            rec(
                2,
                SessionEvent::ItemAdded {
                    item_id: "i2".into(),
                    kind: "plan".into(),
                    payload: json!({"plan": "1. do it"}),
                },
            ),
        ];
        let p = SessionProjection::replay(&log);
        assert_eq!(p.todos.len(), 2);
        assert_eq!(p.todos[0].content, "write code");
        assert_eq!(p.todos[1].status, "pending");
        assert_eq!(p.plan.as_deref(), Some("1. do it"));
    }

    #[test]
    fn failed_turn_records_message() {
        let log = vec![
            rec(
                0,
                SessionEvent::TurnStarted {
                    turn_id: "t".into(),
                    input: "x".into(),
                },
            ),
            rec(
                1,
                SessionEvent::TurnFailed {
                    turn_id: "t".into(),
                    message: "boom".into(),
                },
            ),
        ];
        let p = SessionProjection::replay(&log);
        assert_eq!(p.turns[0].status, TurnStatus::Failed);
        assert_eq!(p.turns[0].output.as_deref(), Some("boom"));
    }

    #[test]
    fn events_without_a_turn_are_ignored_safely() {
        let p = SessionProjection::replay(&[rec(
            0,
            SessionEvent::Step {
                phase: "model".into(),
                label: None,
            },
        )]);
        assert!(p.turns.is_empty());
    }

    #[test]
    fn orphaned_item_and_turn_events_are_ignored_safely() {
        use serde_json::json;
        let p = SessionProjection::replay(&[
            rec(
                0,
                SessionEvent::ItemAdded {
                    item_id: "orphan".into(),
                    kind: "tool".into(),
                    payload: json!({}),
                },
            ),
            rec(
                1,
                SessionEvent::ItemDone {
                    item_id: "orphan".into(),
                    result: "x".into(),
                    is_error: false,
                },
            ),
            rec(
                2,
                SessionEvent::ItemAdded {
                    item_id: "todo".into(),
                    kind: "todo".into(),
                    payload: json!({"unexpected": true}),
                },
            ),
            rec(
                3,
                SessionEvent::TurnCompleted {
                    turn_id: "never-started".into(),
                    output: "x".into(),
                },
            ),
        ]);
        assert!(p.turns.is_empty());
        assert!(p.todos.is_empty());
    }

    #[test]
    fn projection_roundtrips_through_json() {
        let p = SessionProjection::replay(&full_log());
        let json = serde_json::to_string(&p).unwrap();
        let back: SessionProjection = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }
}
