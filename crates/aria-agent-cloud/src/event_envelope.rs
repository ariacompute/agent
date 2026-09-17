//! `AgentEvent` → SSE envelope for the **OpenAI beta Agents** session stream
//! (`POST /v1/agents/sessions/{session_id}/events/stream`).
//!
//! Every frame is an `agent.*` streaming event: `agent.turn.*` for the turn
//! lifecycle and its items, `agent.output.*` for raw command output. Each frame
//! carries `event_id`, `session_id`, `turn_id`, a monotonic `sequence_number`
//! and a `type`.
//!
//! `agent.turn.completed` (or `agent.turn.failed`) is the **only** terminal
//! event — there is no trailing `data: [DONE]` sentinel. Data the beta schema
//! does not model (the agentic `phase` / `label`, the executed tool `result`)
//! rides along as extra fields inside the frames, so strict OpenAI clients
//! ignore them while our own clients can still render a rich timeline.

use agent_core::{AgentEvent, ToolResult};
use serde_json::{json, Value};

/// Streaming event types emitted by `/v1/agents/sessions/{id}/events/stream`.
pub mod event_type {
    pub const TURN_CREATED: &str = "agent.turn.created";
    pub const TURN_IN_PROGRESS: &str = "agent.turn.in_progress";
    pub const TURN_ITEM_ADDED: &str = "agent.turn.item.added";
    pub const TURN_ITEM_DONE: &str = "agent.turn.item.done";
    pub const TURN_OUTPUT_TEXT_DELTA: &str = "agent.turn.output_text.delta";
    pub const TURN_OUTPUT_TEXT_DONE: &str = "agent.turn.output_text.done";
    pub const TURN_COMPLETED: &str = "agent.turn.completed";
    pub const TURN_FAILED: &str = "agent.turn.failed";
    #[allow(dead_code)]
    pub const SESSION_ERROR: &str = "agent.session.error";
    #[allow(dead_code)]
    pub const OUTPUT_COMMAND_DELTA: &str = "agent.output.command_execution_output.delta";
}

/// Message item type (an assistant reply).
const ITEM_MESSAGE: &str = "message";
/// Function-call item type (a tool invocation).
const ITEM_FUNCTION_CALL: &str = "function_call";

/// Stateful translator from the core agentic event stream to wire frames.
///
/// One instance per turn: it owns the monotonic `sequence_number`, the message
/// item id, and the accumulated reply text. `closed` records whether a terminal
/// frame has been emitted — with no `[DONE]` sentinel that is what makes
/// "the last frame is terminal" a hard guarantee.
pub struct EventEnvelope {
    session_id: String,
    agent_id: String,
    turn_id: String,
    seq: u64,
    msg_item_id: String,
    msg_open: bool,
    text: String,
    closed: bool,
}

impl EventEnvelope {
    pub fn new(turn_id: &str, agent_id: &str, session_id: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            agent_id: agent_id.to_string(),
            turn_id: turn_id.to_string(),
            seq: 0,
            msg_item_id: format!("msg_{turn_id}"),
            msg_open: false,
            text: String::new(),
            closed: false,
        }
    }

    /// Opening frame: the turn is created.
    pub fn created(&mut self) -> Value {
        let seq = self.next_seq();
        json!({
            "type": event_type::TURN_CREATED,
            "event_id": format!("event_{seq}"),
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "sequence_number": seq,
            "turn": self.turn_object("in_progress", None),
        })
    }

    /// Translate one core event into zero or more wire frames.
    pub fn push(&mut self, ev: &AgentEvent) -> Vec<Value> {
        match ev {
            AgentEvent::Step { phase, label } => self.step(phase, label.as_deref()),
            AgentEvent::ToolCall {
                id,
                name,
                arguments,
                result,
            } => self.tool_call(id, name, arguments, result),
            AgentEvent::Token { text } => self.token(text),
            AgentEvent::Done { text } => self.finish(text),
        }
    }

    /// Terminal frame on error. `agent.turn.failed` is the only end-of-stream
    /// signal in the failure path.
    pub fn failed(&mut self, message: &str) -> Value {
        let seq = self.next_seq();
        self.closed = true;
        json!({
            "type": event_type::TURN_FAILED,
            "event_id": format!("event_{seq}"),
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "sequence_number": seq,
            "turn": self.turn_object("failed", None),
            "error": { "type": "server_error", "message": message },
        })
    }

    /// Guarantee a terminal frame: if the core stream never produced `Done`,
    /// close the turn here so the client always sees one terminal event.
    pub fn ensure_closed(&mut self) -> Vec<Value> {
        if self.closed {
            return Vec::new();
        }
        self.finish("")
    }

    // ------------------------------------------------------------------ frames

    /// A phase boundary: the model is working, a tool is executing, or the
    /// loop guard tripped. Emitted as `agent.turn.in_progress` with the
    /// agentic `phase` / `label` as extra fields.
    fn step(&mut self, phase: &str, label: Option<&str>) -> Vec<Value> {
        let seq = self.next_seq();
        vec![json!({
            "type": event_type::TURN_IN_PROGRESS,
            "event_id": format!("event_{seq}"),
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "sequence_number": seq,
            "phase": phase,
            "label": label,
            "turn": self.turn_object("in_progress", None),
        })]
    }

    /// A tool invocation: the item is added, then closed with its output.
    fn tool_call(
        &mut self,
        id: &str,
        name: &str,
        arguments: &Value,
        result: &ToolResult,
    ) -> Vec<Value> {
        let added_seq = self.next_seq();
        let done_seq = self.next_seq();
        let item = json!({
            "id": id,
            "type": ITEM_FUNCTION_CALL,
            "status": "in_progress",
            "name": name,
            "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
        });
        let done_item = json!({
            "id": id,
            "type": ITEM_FUNCTION_CALL,
            "status": if result.is_error { "failed" } else { "completed" },
            "name": name,
            "arguments": serde_json::to_string(arguments).unwrap_or_else(|_| "{}".into()),
            // Extra field (not in the beta schema): the executed tool output.
            "result": { "content": result.content, "is_error": result.is_error },
        });
        vec![
            json!({
                "type": event_type::TURN_ITEM_ADDED,
                "event_id": format!("event_{added_seq}"),
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "sequence_number": added_seq,
                "item": item,
            }),
            json!({
                "type": event_type::TURN_ITEM_DONE,
                "event_id": format!("event_{done_seq}"),
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "sequence_number": done_seq,
                "item": done_item,
            }),
        ]
    }

    /// A streamed text delta. Opens the assistant message item on first delta.
    fn token(&mut self, text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.msg_open {
            let seq = self.next_seq();
            self.msg_open = true;
            out.push(json!({
                "type": event_type::TURN_ITEM_ADDED,
                "event_id": format!("event_{seq}"),
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "sequence_number": seq,
                "item": {
                    "id": self.msg_item_id,
                    "type": ITEM_MESSAGE,
                    "status": "in_progress",
                    "role": "assistant",
                    "content": [],
                },
            }));
        }
        self.text.push_str(text);
        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::TURN_OUTPUT_TEXT_DELTA,
            "event_id": format!("event_{seq}"),
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "sequence_number": seq,
            "item_id": self.msg_item_id,
            "delta": text,
        }));
        out
    }

    /// Terminal frames for a successful run: close the text and the message
    /// item, then complete the turn.
    pub fn finish(&mut self, final_text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        // `Done` carries the full reply; when no deltas were streamed (e.g. a
        // tool-only turn that ended with text) open the message first.
        if !final_text.is_empty() && !self.msg_open {
            out.extend(self.token(final_text));
        }
        if !final_text.is_empty() {
            self.text = final_text.to_string();
        }
        if self.msg_open {
            let seq = self.next_seq();
            out.push(json!({
                "type": event_type::TURN_OUTPUT_TEXT_DONE,
                "event_id": format!("event_{seq}"),
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "sequence_number": seq,
                "item_id": self.msg_item_id,
                "text": self.text,
            }));
            let seq = self.next_seq();
            out.push(json!({
                "type": event_type::TURN_ITEM_DONE,
                "event_id": format!("event_{seq}"),
                "session_id": self.session_id,
                "turn_id": self.turn_id,
                "sequence_number": seq,
                "item": {
                    "id": self.msg_item_id,
                    "type": ITEM_MESSAGE,
                    "status": "completed",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": self.text }],
                },
            }));
            self.msg_open = false;
        }
        let seq = self.next_seq();
        self.closed = true;
        out.push(json!({
            "type": event_type::TURN_COMPLETED,
            "event_id": format!("event_{seq}"),
            "session_id": self.session_id,
            "turn_id": self.turn_id,
            "sequence_number": seq,
            "turn": self.turn_object("completed", Some(&self.text.clone())),
        }));
        out
    }

    fn turn_object(&self, status: &str, output: Option<&str>) -> Value {
        json!({
            "id": self.turn_id,
            "object": "agent.session.turn",
            "session_id": self.session_id,
            "agent_id": self.agent_id,
            "status": status,
            "output": output,
        })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> EventEnvelope {
        EventEnvelope::new("turn-1", "agent-demo", "sess-1")
    }

    fn types(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["type"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn created_frame_carries_turn_identity() {
        let mut env = envelope();
        let frame = env.created();
        assert_eq!(frame["type"], json!(event_type::TURN_CREATED));
        assert_eq!(frame["sequence_number"], json!(1));
        assert_eq!(frame["session_id"], json!("sess-1"));
        assert_eq!(frame["turn_id"], json!("turn-1"));
        assert_eq!(frame["turn"]["status"], json!("in_progress"));
        assert_eq!(frame["turn"]["agent_id"], json!("agent-demo"));
    }

    #[test]
    fn step_maps_to_in_progress_with_phase() {
        let mut env = envelope();
        let frames = env.push(&AgentEvent::Step {
            phase: "tool_exec".into(),
            label: Some("shell".into()),
        });
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], json!(event_type::TURN_IN_PROGRESS));
        assert_eq!(frames[0]["phase"], json!("tool_exec"));
        assert_eq!(frames[0]["label"], json!("shell"));
    }

    #[test]
    fn token_opens_message_then_streams_deltas() {
        let mut env = envelope();
        let first = env.push(&AgentEvent::Token { text: "he".into() });
        assert_eq!(
            types(&first),
            vec![
                event_type::TURN_ITEM_ADDED,
                event_type::TURN_OUTPUT_TEXT_DELTA
            ]
        );
        assert_eq!(first[0]["item"]["type"], json!(ITEM_MESSAGE));
        assert_eq!(first[1]["delta"], json!("he"));

        // The second delta does not re-open the message.
        let second = env.push(&AgentEvent::Token { text: "llo".into() });
        assert_eq!(types(&second), vec![event_type::TURN_OUTPUT_TEXT_DELTA]);
    }

    #[test]
    fn tool_call_emits_item_added_and_done_with_result() {
        let mut env = envelope();
        let frames = env.push(&AgentEvent::ToolCall {
            id: "call_1".into(),
            name: "shell".into(),
            arguments: json!({ "command": ["echo", "hi"] }),
            result: ToolResult {
                call_id: "call_1".into(),
                content: "hi\n".into(),
                is_error: false,
            },
        });
        assert_eq!(
            types(&frames),
            vec![event_type::TURN_ITEM_ADDED, event_type::TURN_ITEM_DONE]
        );
        assert_eq!(frames[0]["item"]["name"], json!("shell"));
        assert_eq!(frames[1]["item"]["status"], json!("completed"));
        assert_eq!(frames[1]["item"]["result"]["content"], json!("hi\n"));
    }

    #[test]
    fn failed_tool_call_marks_item_failed() {
        let mut env = envelope();
        let frames = env.push(&AgentEvent::ToolCall {
            id: "c".into(),
            name: "shell".into(),
            arguments: json!({}),
            result: ToolResult {
                call_id: "c".into(),
                content: "boom".into(),
                is_error: true,
            },
        });
        assert_eq!(frames[1]["item"]["status"], json!("failed"));
    }

    #[test]
    fn done_closes_message_and_completes_turn() {
        let mut env = envelope();
        env.push(&AgentEvent::Token {
            text: "hello".into(),
        });
        let close = env.push(&AgentEvent::Done {
            text: "hello".into(),
        });
        assert_eq!(
            types(&close),
            vec![
                event_type::TURN_OUTPUT_TEXT_DONE,
                event_type::TURN_ITEM_DONE,
                event_type::TURN_COMPLETED,
            ]
        );
        assert_eq!(close[0]["text"], json!("hello"));
        assert_eq!(close[1]["item"]["content"][0]["text"], json!("hello"));
        assert_eq!(close[2]["turn"]["status"], json!("completed"));
        assert_eq!(close[2]["turn"]["output"], json!("hello"));
    }

    #[test]
    fn completed_is_the_only_terminal_frame() {
        let mut env = envelope();
        env.push(&AgentEvent::Done { text: "hi".into() });
        // `ensure_closed` must not append a second terminal frame.
        assert!(env.ensure_closed().is_empty());
    }

    #[test]
    fn ensure_closed_terminates_a_stream_without_done() {
        let mut env = envelope();
        env.push(&AgentEvent::Token {
            text: "partial".into(),
        });
        let frames = env.ensure_closed();
        assert_eq!(types(&frames).last().unwrap(), event_type::TURN_COMPLETED);
        assert_eq!(frames.last().unwrap()["turn"]["output"], json!("partial"));
    }

    #[test]
    fn failed_is_terminal_and_carries_the_error() {
        let mut env = envelope();
        let frame = env.failed("boom");
        assert_eq!(frame["type"], json!(event_type::TURN_FAILED));
        assert_eq!(frame["turn"]["status"], json!("failed"));
        assert_eq!(frame["error"]["message"], json!("boom"));
        // Already terminal: no extra frames are appended afterwards.
        assert!(env.ensure_closed().is_empty());
    }

    #[test]
    fn sequence_numbers_are_monotonic() {
        let mut env = envelope();
        let mut seqs = vec![env.created()["sequence_number"].as_u64().unwrap()];
        for ev in [
            AgentEvent::Step {
                phase: "model".into(),
                label: None,
            },
            AgentEvent::Token { text: "a".into() },
            AgentEvent::Done { text: "a".into() },
        ] {
            for f in env.push(&ev) {
                seqs.push(f["sequence_number"].as_u64().unwrap());
            }
        }
        assert_eq!(seqs, vec![1, 2, 3, 4, 5, 6, 7]);
        assert!(seqs.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn every_frame_carries_session_and_turn_ids() {
        let mut env = envelope();
        let mut frames = vec![env.created()];
        frames.extend(env.push(&AgentEvent::Token { text: "x".into() }));
        frames.extend(env.push(&AgentEvent::Done { text: "x".into() }));
        for f in frames {
            assert_eq!(f["session_id"], json!("sess-1"));
            assert_eq!(f["turn_id"], json!("turn-1"));
            assert!(f["event_id"].is_string());
        }
    }
}
