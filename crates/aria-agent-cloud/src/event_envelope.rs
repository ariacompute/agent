//! `AgentEvent` → SSE envelope: the frozen wire contract between agent-cloud
//! and every consumer (SDKs, the playground Agent playground, raw HTTP).
//!
//! Every frame carries an **OpenAI Responses API streaming `type`**, so OpenAI
//! SDKs and the Agents SDK can consume `/v1/sessions/:id/runs/stream` unchanged.
//! Data the Responses schema does not model (the agentic phase, the executed
//! tool result, the Reef receipt id) is carried as **extra fields inside** those
//! OpenAI-shaped frames — never as private `aria.*` frames — so a strict OpenAI
//! client ignores them while our own clients can still render a rich timeline.
//!
//! The OpenAI Responses streaming event sequence is followed faithfully:
//! `response.created` → `response.output_item.added` → `response.content_part.added`
//! → `response.output_text.delta` (×N) → `response.content_part.done` →
//! `response.output_text.done` → `response.output_item.done` → `response.completed`.
//! `function_call` items emit `response.function_call_arguments.delta` instead of
//! text parts. `response.failed` is the terminal event on error.

use agent_core::{AgentEvent, ToolResult};
use serde_json::{json, Value};

/// OpenAI Responses API streaming event types emitted by
/// `/v1/sessions/:id/runs/stream`.
pub mod event_type {
    pub const CREATED: &str = "response.created";
    pub const IN_PROGRESS: &str = "response.in_progress";
    pub const OUTPUT_ITEM_ADDED: &str = "response.output_item.added";
    pub const CONTENT_PART_ADDED: &str = "response.content_part.added";
    pub const OUTPUT_TEXT_DELTA: &str = "response.output_text.delta";
    pub const CONTENT_PART_DONE: &str = "response.content_part.done";
    pub const OUTPUT_TEXT_DONE: &str = "response.output_text.done";
    pub const OUTPUT_ITEM_DONE: &str = "response.output_item.done";
    pub const FUNCTION_CALL_ARGUMENTS_DELTA: &str = "response.function_call_arguments.delta";
    pub const COMPLETED: &str = "response.completed";
    pub const FAILED: &str = "response.failed";
}

/// Max bytes per `function_call_arguments.delta` chunk.
const ARGS_CHUNK: usize = 64;

/// Index of the single text content part of an assistant message item.
const CONTENT_INDEX: u32 = 0;

/// Stateful translator from the core agentic event stream to wire frames.
///
/// One instance per run: it owns the monotonic `sequence_number`, the
/// assistant message item id, and the accumulated reply text. Splitting
/// `open_message` / `close_message` keeps the message lifecycle explicit and
/// the text accumulator local to those two helpers.
pub struct EventEnvelope {
    run_id: String,
    agent_id: String,
    session: String,
    record_id: String,
    seq: u64,
    msg_item_id: String,
    msg_open: bool,
    text: String,
    // Set once a terminal frame (`response.completed` / `response.failed`) has
    // been emitted. With no `[DONE]` sentinel, this is what makes "the last
    // frame is terminal" a hard guarantee.
    closed: bool,
}

impl EventEnvelope {
    pub fn new(run_id: &str, agent_id: &str, session: &str, record_id: &str) -> Self {
        Self {
            run_id: run_id.to_string(),
            agent_id: agent_id.to_string(),
            session: session.to_string(),
            record_id: record_id.to_string(),
            seq: 0,
            msg_item_id: format!("msg_{run_id}"),
            msg_open: false,
            text: String::new(),
            closed: false,
        }
    }

    /// Opening frame: run identity + Reef receipt id under `response.metadata`.
    pub fn created(&mut self) -> Value {
        let seq = self.next_seq();
        let response = self.response_object("in_progress");
        json!({
            "type": event_type::CREATED,
            "sequence_number": seq,
            "response": response,
        })
    }

    /// Translate one core event into zero or more wire frames.
    pub fn push(&mut self, event: &AgentEvent) -> Vec<Value> {
        match event {
            AgentEvent::Step { phase, label } => vec![self.step(phase, label.as_deref())],
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

    /// Terminal frames for a successful run: close the assistant message, then
    /// complete the response carrying the Reef receipt id.
    pub fn finish(&mut self, final_text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        // `Done` carries the full reply; when no deltas were streamed (e.g. a
        // tool-only turn that ended with text) open the message first.
        if !final_text.is_empty() && !self.msg_open {
            out.extend(self.token(final_text));
        }
        out.extend(self.close_message());
        out.push(self.completed());
        self.closed = true;
        out
    }

    /// Terminal frame for a failed run (still OpenAI-shaped so the client's
    /// stream parser terminates cleanly).
    pub fn failed(&mut self, message: &str) -> Value {
        let seq = self.next_seq();
        let mut response = self.response_object("failed");
        response["error"] = json!({ "code": "server_error", "message": message });
        self.closed = true;
        self.msg_open = false;
        json!({
            "type": event_type::FAILED,
            "sequence_number": seq,
            "response": response,
        })
    }

    /// Guarantees the stream always ends on a terminal event.
    ///
    /// `response.completed` (or `response.failed`) is the *only* end-of-stream
    /// signal a client keys on. If the core event stream ended without producing
    /// a terminal frame, emit `response.completed` so the contract — "the last
    /// frame is always a terminal one" — holds unconditionally. Returns an empty
    /// vec when a terminal frame was already emitted.
    pub fn ensure_closed(&mut self) -> Vec<Value> {
        if self.closed {
            return Vec::new();
        }
        self.finish("")
    }

    // --- frame builders ---

    /// Phase boundary (`recall` / `model` / `tool_exec` / `loop_guard`).
    /// `phase` + `label` are extra fields; OpenAI clients ignore them.
    fn step(&mut self, phase: &str, label: Option<&str>) -> Value {
        let seq = self.next_seq();
        json!({
            "type": event_type::IN_PROGRESS,
            "sequence_number": seq,
            "phase": phase,
            "label": label,
        })
    }

    /// One tool invocation: item added → argument deltas → item done (the
    /// executed result rides along as the item's extra `result` field).
    fn tool_call(
        &mut self,
        id: &str,
        name: &str,
        arguments: &Value,
        result: &ToolResult,
    ) -> Vec<Value> {
        let args = serde_json::to_string(arguments).unwrap_or_else(|_| "{}".to_string());
        let mut out = Vec::new();

        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::OUTPUT_ITEM_ADDED,
            "sequence_number": seq,
            "item": {
                "id": id,
                "type": "function_call",
                "status": "in_progress",
                "name": name,
                "arguments": "",
                "call_id": id,
            },
        }));

        for chunk in chunked(&args) {
            let seq = self.next_seq();
            out.push(json!({
                "type": event_type::FUNCTION_CALL_ARGUMENTS_DELTA,
                "sequence_number": seq,
                "item_id": id,
                "delta": chunk,
            }));
        }

        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::OUTPUT_ITEM_DONE,
            "sequence_number": seq,
            "item": {
                "id": id,
                "type": "function_call",
                "status": "completed",
                "name": name,
                "arguments": args,
                "call_id": id,
                "result": result,
            },
        }));
        out
    }

    /// Open the assistant message item (empty `content`); the text part is added
    /// by `content_part_added` immediately after.
    fn open_message(&mut self) -> Value {
        self.msg_open = true;
        let seq = self.next_seq();
        json!({
            "type": event_type::OUTPUT_ITEM_ADDED,
            "sequence_number": seq,
            "item": {
                "id": self.msg_item_id,
                "type": "message",
                "status": "in_progress",
                "role": "assistant",
                "content": [],
            },
        })
    }

    /// Announce the single `output_text` content part of the message item.
    fn content_part_added(&mut self) -> Value {
        let seq = self.next_seq();
        json!({
            "type": event_type::CONTENT_PART_ADDED,
            "sequence_number": seq,
            "item_id": self.msg_item_id,
            "content_index": CONTENT_INDEX,
            "part": { "type": "output_text", "text": "", "annotations": [] },
        })
    }

    /// A model text delta. Opens the message + content part on the first delta.
    fn token(&mut self, text: &str) -> Vec<Value> {
        let mut out = Vec::new();
        if !self.msg_open {
            out.push(self.open_message());
            out.push(self.content_part_added());
        }
        self.text.push_str(text);
        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::OUTPUT_TEXT_DELTA,
            "sequence_number": seq,
            "item_id": self.msg_item_id,
            "content_index": CONTENT_INDEX,
            "delta": text,
        }));
        out
    }

    /// Close the assistant message item: `content_part.done` → `output_text.done`
    /// → `output_item.done`, carrying the full accumulated text.
    fn close_message(&mut self) -> Vec<Value> {
        if !self.msg_open {
            return Vec::new();
        }
        self.msg_open = false;
        let mut out = Vec::new();
        let text = self.text.clone();

        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::CONTENT_PART_DONE,
            "sequence_number": seq,
            "item_id": self.msg_item_id,
            "content_index": CONTENT_INDEX,
            "part": { "type": "output_text", "text": text.clone(), "annotations": [] },
        }));

        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::OUTPUT_TEXT_DONE,
            "sequence_number": seq,
            "item_id": self.msg_item_id,
            "content_index": CONTENT_INDEX,
            "text": text.clone(),
        }));

        let seq = self.next_seq();
        out.push(json!({
            "type": event_type::OUTPUT_ITEM_DONE,
            "sequence_number": seq,
            "item": {
                "id": self.msg_item_id,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text, "annotations": [] }],
            },
        }));
        out
    }

    fn completed(&mut self) -> Value {
        let seq = self.next_seq();
        let response = self.response_object("completed");
        json!({
            "type": event_type::COMPLETED,
            "sequence_number": seq,
            "response": response,
        })
    }

    fn response_object(&self, status: &str) -> Value {
        json!({
            "id": self.run_id,
            "object": "response",
            "status": status,
            "metadata": {
                "agent_id": self.agent_id,
                "session": self.session,
                "reef_record_id": self.record_id,
            },
        })
    }

    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }
}

/// Split `s` into UTF-8-safe chunks so argument deltas never split a char.
fn chunked(s: &str) -> Vec<&str> {
    if s.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut start = 0;
    while start < s.len() {
        let mut end = (start + ARGS_CHUNK).min(s.len());
        while end > start && !s.is_char_boundary(end) {
            end -= 1;
        }
        out.push(&s[start..end]);
        start = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn envelope() -> EventEnvelope {
        EventEnvelope::new("run-1", "agent-demo", "default", "rec-1")
    }

    fn types(frames: &[Value]) -> Vec<String> {
        frames
            .iter()
            .filter_map(|f| f["type"].as_str().map(str::to_string))
            .collect()
    }

    #[test]
    fn created_frame_carries_run_metadata_and_receipt() {
        let mut env = envelope();
        let frame = env.created();
        assert_eq!(frame["type"], json!(event_type::CREATED));
        assert_eq!(frame["sequence_number"], json!(1));
        assert_eq!(frame["response"]["id"], json!("run-1"));
        assert_eq!(frame["response"]["object"], json!("response"));
        assert_eq!(frame["response"]["status"], json!("in_progress"));
        assert_eq!(
            frame["response"]["metadata"]["reef_record_id"],
            json!("rec-1")
        );
        assert_eq!(
            frame["response"]["metadata"]["agent_id"],
            json!("agent-demo")
        );
    }

    #[test]
    fn step_maps_to_in_progress_with_phase() {
        let mut env = envelope();
        let frames = env.push(&AgentEvent::Step {
            phase: "tool_exec".into(),
            label: Some("shell".into()),
        });
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0]["type"], json!(event_type::IN_PROGRESS));
        assert_eq!(frames[0]["phase"], json!("tool_exec"));
        assert_eq!(frames[0]["label"], json!("shell"));
    }

    #[test]
    fn tool_call_streams_arguments_then_result() {
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
        // function_call items emit no content_part frames.
        assert_eq!(
            types(&frames),
            vec![
                event_type::OUTPUT_ITEM_ADDED,
                event_type::FUNCTION_CALL_ARGUMENTS_DELTA,
                event_type::OUTPUT_ITEM_DONE,
            ]
        );
        assert_eq!(frames[0]["item"]["type"], json!("function_call"));
        assert_eq!(frames[0]["item"]["call_id"], json!("call_1"));
        // The executed result rides along on the completed item.
        assert_eq!(frames[2]["item"]["result"]["content"], json!("hi\n"));
        assert_eq!(frames[2]["item"]["result"]["is_error"], json!(false));
    }

    #[test]
    fn token_streams_content_part_then_text_delta() {
        let mut env = envelope();
        let open = env.push(&AgentEvent::Token { text: "he".into() });
        // First delta opens the message, announces the text part, then streams.
        assert_eq!(
            types(&open),
            vec![
                event_type::OUTPUT_ITEM_ADDED,
                event_type::CONTENT_PART_ADDED,
                event_type::OUTPUT_TEXT_DELTA,
            ]
        );
        assert_eq!(open[0]["item"]["type"], json!("message"));
        assert_eq!(open[0]["item"]["content"], json!([]));
        assert_eq!(open[1]["part"]["type"], json!("output_text"));
        assert_eq!(open[2]["delta"], json!("he"));

        let frames = env.push(&AgentEvent::Token { text: "llo".into() });
        // Already open: only the delta is emitted.
        assert_eq!(types(&frames), vec![event_type::OUTPUT_TEXT_DELTA]);
        assert_eq!(frames[0]["delta"], json!("llo"));

        let close = env.push(&AgentEvent::Done {
            text: "hello".into(),
        });
        assert_eq!(
            types(&close),
            vec![
                event_type::CONTENT_PART_DONE,
                event_type::OUTPUT_TEXT_DONE,
                event_type::OUTPUT_ITEM_DONE,
                event_type::COMPLETED,
            ]
        );
        // The full text is carried on both done frames and the completed item.
        assert_eq!(close[0]["part"]["text"], json!("hello"));
        assert_eq!(close[1]["text"], json!("hello"));
        assert_eq!(close[2]["item"]["content"][0]["text"], json!("hello"));
        assert_eq!(close[3]["response"]["status"], json!("completed"));
        assert_eq!(
            close[3]["response"]["metadata"]["reef_record_id"],
            json!("rec-1")
        );
    }

    #[test]
    fn done_without_prior_delta_opens_and_closes_message() {
        let mut env = envelope();
        let frames = env.push(&AgentEvent::Done {
            text: "hello".into(),
        });
        assert_eq!(
            types(&frames),
            vec![
                event_type::OUTPUT_ITEM_ADDED,
                event_type::CONTENT_PART_ADDED,
                event_type::OUTPUT_TEXT_DELTA,
                event_type::CONTENT_PART_DONE,
                event_type::OUTPUT_TEXT_DONE,
                event_type::OUTPUT_ITEM_DONE,
                event_type::COMPLETED,
            ]
        );
    }

    #[test]
    fn no_private_frames_leak_into_the_stream() {
        let mut env = envelope();
        let mut all = vec![env.created()];
        all.extend(env.push(&AgentEvent::Step {
            phase: "recall".into(),
            label: None,
        }));
        all.extend(env.push(&AgentEvent::ToolCall {
            id: "call_1".into(),
            name: "shell".into(),
            arguments: json!({ "command": ["ls"] }),
            result: ToolResult {
                call_id: "call_1".into(),
                content: String::new(),
                is_error: true,
            },
        }));
        all.extend(env.push(&AgentEvent::Token { text: "ok".into() }));
        all.extend(env.push(&AgentEvent::Done { text: "ok".into() }));

        assert!(!all.is_empty());
        for frame in &all {
            let t = frame["type"].as_str().expect("frame must carry a type");
            assert!(
                t.starts_with("response."),
                "private (non-OpenAI) frame leaked: {t}"
            );
            assert!(
                frame.get("sequence_number").is_some(),
                "frame {t} missing sequence_number"
            );
        }
    }

    #[test]
    fn failed_frame_is_openai_shaped() {
        let mut env = envelope();
        let frame = env.failed("boom");
        assert_eq!(frame["type"], json!(event_type::FAILED));
        assert_eq!(frame["response"]["status"], json!("failed"));
        assert_eq!(frame["response"]["error"]["message"], json!("boom"));
    }

    // There is no `[DONE]` sentinel: `response.completed` / `response.failed`
    // is the only end-of-stream signal, so the last frame MUST always be a
    // terminal one.
    #[test]
    fn last_frame_is_always_terminal() {
        // Normal completion via `Done`.
        let mut env = envelope();
        let frames = env.push(&AgentEvent::Done { text: "ok".into() });
        assert_eq!(frames.last().unwrap()["type"], json!(event_type::COMPLETED));
        // Already terminal: a second call must not emit a duplicate frame.
        assert!(env.ensure_closed().is_empty());

        // Stream that ends without ever producing `Done`.
        let mut env = envelope();
        env.push(&AgentEvent::Token {
            text: "partial".into(),
        });
        let frames = env.ensure_closed();
        assert_eq!(frames.last().unwrap()["type"], json!(event_type::COMPLETED));
        // Now closed, so a further call is a no-op.
        assert!(env.ensure_closed().is_empty());

        // A failed run is terminal too, and closes the run.
        let mut env = envelope();
        let frame = env.failed("boom");
        assert_eq!(frame["type"], json!(event_type::FAILED));
        assert!(env.ensure_closed().is_empty());
    }

    #[test]
    fn argument_deltas_never_split_utf8() {
        let s = "é".repeat(100);
        let joined: String = chunked(&s).concat();
        assert_eq!(joined, s);
        assert!(chunked("").is_empty());
    }
}
