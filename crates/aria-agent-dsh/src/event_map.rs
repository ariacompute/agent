//! dsh notification → wire-neutral event mapping.
//!
//! dsh is in developer preview and its `SessionEvent` payloads are documented as
//! "shapes defined by the generated Cordis catalog", not as a frozen schema. That
//! risk is contained here: **one** pure function (plus its extraction helpers)
//! translates the frames, so a dsh upgrade touches this file only.
//!
//! Mapping contract with the rest of the runtime:
//!
//! * `agent.turn.*` frames are produced downstream by
//!   `aria-agent-cloud::event_envelope`; this module never emits them.
//! * Only [`Mapped`] values come out — no session-recorder or seam types — so it
//!   is exhaustively unit-testable without a child process.
//! * Unknown/transient frames degrade to [`Mapped::Ignore`]: a newer dsh may add
//!   events, and an unknown notification must never abort a turn. A *terminal*
//!   frame with a missing `reason.kind` is a protocol error (see
//!   [`Mapped::Protocol`]) because silently inventing a finish reason would hide
//!   a truncated answer.

use serde_json::Value;

/// One normalized step of a dsh turn.
#[derive(Debug, Clone, PartialEq)]
pub enum Mapped {
    /// A progress step (`phase` is a stable short label).
    Step {
        phase: &'static str,
        label: Option<String>,
    },
    /// A completed tool call.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        content: String,
        is_error: bool,
    },
    /// Assistant text committed for this turn.
    Token { text: String },
    /// Provider-reported token accounting.
    Usage {
        input_tokens: u64,
        output_tokens: u64,
        model: String,
    },
    /// Terminal frame: the turn finished.
    TurnEnd { reason: String },
    /// Terminal frame: the turn failed.
    TurnFailed { message: String },
    /// A frame this build does not model (added by a newer dsh).
    Ignore,
    /// A terminal frame that violated the wire contract.
    Protocol { message: String },
}

/// Map one notification frame to zero or more normalized steps.
pub fn map_notification(frame: &Value) -> Vec<Mapped> {
    let method = frame.get("method").and_then(Value::as_str).unwrap_or("");
    let params = frame.get("params").unwrap_or(&Value::Null);
    // dsh 0.2.x delivers every session event as one `session.event` envelope:
    // `params.event.type` plus `params.event.data`. Older builds used the event
    // type as the notification method; both shapes land in `map_event`.
    if method == "session.event" {
        let Some(event) = params.get("event") else {
            return vec![Mapped::Ignore];
        };
        let kind = event.get("type").and_then(Value::as_str).unwrap_or("");
        let data = event.get("data").unwrap_or(&Value::Null);
        return map_event(kind, data);
    }
    map_event(method, params)
}

fn map_event(method: &str, data: &Value) -> Vec<Mapped> {
    match method {
        "turn/start" => vec![Mapped::Step {
            phase: "harness",
            label: None,
        }],
        "step/start" => vec![Mapped::Step {
            phase: "step",
            label: None,
        }],
        "tool/call" => {
            let name = string_at(data, &["call", "name"]).unwrap_or_else(|| "tool".into());
            vec![Mapped::Step {
                phase: "tool_exec",
                label: Some(name),
            }]
        }
        "tool/result" => {
            let result = data.get("result").unwrap_or(data);
            vec![Mapped::ToolCall {
                id: string_at(data, &["call", "id"])
                    .or_else(|| string_at(data, &["callId"]))
                    .unwrap_or_default(),
                name: string_at(data, &["call", "name"]).unwrap_or_else(|| "tool".into()),
                arguments: data
                    .get("call")
                    .and_then(|c| c.get("arguments"))
                    .cloned()
                    .unwrap_or(Value::Null),
                content: text_of(result),
                is_error: result
                    .get("isError")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            }]
        }
        "assistant/message" | "assistant/message/committed" => {
            // Usage is emitted **before** the token, matching the native loop's
            // invariant (`Usage` → `Token` → `Done`).
            let mut out = Vec::new();
            if let Some(usage) = usage_of(data) {
                out.push(usage);
            }
            let text = text_of(data);
            if !text.is_empty() {
                out.push(Mapped::Token { text });
            }
            if out.is_empty() {
                out.push(Mapped::Ignore);
            }
            out
        }
        "turn/end" => match string_at(data, &["reason", "kind"]) {
            Some(reason) => vec![Mapped::TurnEnd { reason }],
            None => vec![Mapped::Protocol {
                message: "dsh `turn/end` is missing `reason.kind`".into(),
            }],
        },
        "error" | "agent/error" | "agent/request-error" => vec![Mapped::TurnFailed {
            message: string_at(data, &["message"])
                .or_else(|| string_at(data, &["error", "message"]))
                .unwrap_or_else(|| "dsh reported an error".into()),
        }],
        _ => vec![Mapped::Ignore],
    }
}

/// Extract prompt text from a payload shaped as a string, a list of content
/// blocks, or an object holding either under `text`/`content`.
pub fn text_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(text_of)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(""),
        Value::Object(map) => ["text", "content", "message"]
            .iter()
            .map(|key| map.get(*key).map(text_of).unwrap_or_default())
            .find(|s| !s.is_empty())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

fn string_at(value: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    cursor.as_str().map(str::to_string)
}

fn usage_of(data: &Value) -> Option<Mapped> {
    let usage = data.get("usage")?;
    let number = |keys: &[&str]| -> u64 {
        keys.iter()
            .find_map(|k| usage.get(*k).and_then(Value::as_u64))
            .unwrap_or(0)
    };
    Some(Mapped::Usage {
        input_tokens: number(&["inputTokens", "input_tokens"]),
        output_tokens: number(&["outputTokens", "output_tokens"]),
        model: usage
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
    })
}

/// Keep provider failure details bounded: a message may carry URLs or long
/// payload fragments, and it ends up in SSE errors visible to SDK callers.
const FAILURE_DETAIL_MAX_CHARS: usize = 512;

/// Extract a bounded, human-readable failure detail from one dsh notification.
///
/// dsh reports failures in three shapes: `assistant/attempt` stream chunks,
/// the terminal `turn/end` reason, and older builds that put `reason` directly
/// under `params`. Unknown shapes return `None` so callers keep their generic
/// fail-closed message.
pub fn failure_detail(frame: &Value) -> Option<String> {
    let params = frame.get("params")?;
    let event = params.get("event").unwrap_or(params);
    let kind = event
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| frame.get("method").and_then(Value::as_str))
        .unwrap_or("");
    let legacy_failure = || params.get("reason")?.get("failure");
    let failure = match kind {
        "assistant/attempt" => event
            .get("data")
            .and_then(|data| data.get("stream"))
            .and_then(Value::as_array)
            .and_then(|chunks| {
                chunks
                    .iter()
                    .find_map(|chunk| chunk.get("chunk")?.get("reason")?.get("failure"))
            })
            .or_else(legacy_failure),
        "turn/end" => event
            .get("data")
            .and_then(|data| data.get("reason"))
            .and_then(|reason| reason.get("failure"))
            .or_else(legacy_failure),
        _ => legacy_failure(),
    }?;
    let code = failure.get("code").and_then(Value::as_str).unwrap_or("");
    let message = failure.get("message").and_then(Value::as_str).unwrap_or("");
    let mut body = [code, message]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if body.is_empty() {
        return None;
    }
    if !kind.is_empty() && kind != "session.event" {
        body = format!("{kind}: {body}");
    }
    let truncated = body.chars().count() > FAILURE_DETAIL_MAX_CHARS;
    let mut out: String = body.chars().take(FAILURE_DETAIL_MAX_CHARS).collect();
    if truncated {
        out.push('…');
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn mapped(method: &str, params: Value) -> Vec<Mapped> {
        map_notification(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
    }

    #[test]
    fn lifecycle_frames_become_steps() {
        assert_eq!(
            mapped("turn/start", json!({})),
            vec![Mapped::Step {
                phase: "harness",
                label: None
            }]
        );
        assert_eq!(
            mapped("step/start", json!({})),
            vec![Mapped::Step {
                phase: "step",
                label: None
            }]
        );
    }

    #[test]
    fn tool_frames_carry_name_arguments_and_content() {
        assert_eq!(
            mapped("tool/call", json!({"call": {"id": "c1", "name": "shell"}})),
            vec![Mapped::Step {
                phase: "tool_exec",
                label: Some("shell".into())
            }]
        );
        let out = mapped(
            "tool/result",
            json!({"call": {"id": "c1", "name": "shell", "arguments": {"command": ["ls"]}},
                   "result": {"content": "exit=0", "isError": false}}),
        );
        assert_eq!(
            out,
            vec![Mapped::ToolCall {
                id: "c1".into(),
                name: "shell".into(),
                arguments: json!({"command": ["ls"]}),
                content: "exit=0".into(),
                is_error: false,
            }]
        );
        // Missing name/id degrade to placeholders instead of panicking.
        let bare = mapped("tool/result", json!({"callId": "c2", "result": "boom"}));
        assert_eq!(
            bare,
            vec![Mapped::ToolCall {
                id: "c2".into(),
                name: "tool".into(),
                arguments: Value::Null,
                content: "boom".into(),
                is_error: false,
            }]
        );
        // `isError` may sit on the result object itself.
        let failed = mapped(
            "tool/result",
            json!({"call": {"id": "c3", "name": "shell"}, "result": {"content": "x", "isError": true}}),
        );
        assert!(matches!(
            &failed[0],
            Mapped::ToolCall { is_error: true, .. }
        ));
    }

    #[test]
    fn assistant_messages_yield_text_and_optional_usage() {
        let out = mapped(
            "assistant/message",
            json!({"content": [{"type": "text", "text": "hello "}, {"type": "text", "text": "world"}],
                   "usage": {"inputTokens": 12, "outputTokens": 3, "model": "deepseek-v4"}}),
        );
        assert_eq!(
            out,
            vec![
                Mapped::Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    model: "deepseek-v4".into()
                },
                Mapped::Token {
                    text: "hello world".into()
                },
            ]
        );
        // Snake-case usage and a committed alias are accepted.
        let out = mapped(
            "assistant/message/committed",
            json!({"message": {"content": "hi"}, "usage": {"input_tokens": 1, "output_tokens": 2}}),
        );
        assert_eq!(
            out[0],
            Mapped::Usage {
                input_tokens: 1,
                output_tokens: 2,
                model: String::new()
            }
        );
        assert_eq!(out[1], Mapped::Token { text: "hi".into() });
        // A content-less assistant message is legal (max-tokens) and ignored.
        assert_eq!(
            mapped("assistant/message", json!({"content": []})),
            vec![Mapped::Ignore]
        );
    }

    #[test]
    fn session_event_envelopes_are_unwrapped() {
        let out = mapped(
            "session.event",
            json!({"sessionId": "s1", "event": {
                "type": "assistant/message",
                "data": {"message": {"content": [{"type": "text", "text": "pong"}]}}
            }}),
        );
        assert_eq!(
            out,
            vec![Mapped::Token {
                text: "pong".into()
            }]
        );

        assert_eq!(
            mapped(
                "session.event",
                json!({"sessionId": "s1", "event": {
                    "type": "turn/end",
                    "data": {"reason": {"kind": "completed"}}
                }}),
            ),
            vec![Mapped::TurnEnd {
                reason: "completed".into()
            }]
        );
        assert_eq!(
            mapped("session.event", json!({"sessionId": "s1"})),
            vec![Mapped::Ignore]
        );
    }

    #[test]
    fn terminal_frames_and_protocol_violations() {
        assert_eq!(
            mapped("turn/end", json!({"reason": {"kind": "completed"}})),
            vec![Mapped::TurnEnd {
                reason: "completed".into()
            }]
        );
        assert!(matches!(
            &mapped("turn/end", json!({"reason": {}}))[0],
            Mapped::Protocol { message } if message.contains("reason.kind")
        ));
        assert_eq!(
            mapped("error", json!({"message": "boom"})),
            vec![Mapped::TurnFailed {
                message: "boom".into()
            }]
        );
        assert_eq!(
            mapped("agent/error", json!({"error": {"message": "nested"}})),
            vec![Mapped::TurnFailed {
                message: "nested".into()
            }]
        );
        assert_eq!(
            mapped("agent/request-error", json!({})),
            vec![Mapped::TurnFailed {
                message: "dsh reported an error".into()
            }]
        );
    }

    #[test]
    fn unknown_and_transient_frames_are_ignored() {
        assert_eq!(
            mapped("agent/assistant-stream", json!({"delta": "x"})),
            vec![Mapped::Ignore]
        );
        assert_eq!(mapped("command/run", json!({})), vec![Mapped::Ignore]);
        assert_eq!(map_notification(&json!({})), vec![Mapped::Ignore]);
    }

    #[test]
    fn text_extraction_handles_every_shape() {
        assert_eq!(text_of(&json!("plain")), "plain");
        assert_eq!(text_of(&json!({"text": "a"})), "a");
        assert_eq!(text_of(&json!({"content": "b"})), "b");
        assert_eq!(text_of(&json!({"message": {"content": "c"}})), "c");
        assert_eq!(text_of(&json!([{"text": "d"}, "e"])), "de");
        assert_eq!(text_of(&json!(42)), "");
        assert_eq!(text_of(&json!({})), "");
        assert_eq!(string_at(&json!({"a": {"b": 1}}), &["a", "b"]), None);
        assert_eq!(string_at(&json!({"a": "x"}), &["a"]).as_deref(), Some("x"));
    }

    #[test]
    fn failure_detail_extracts_every_dsh_shape() {
        let attempt = json!({
            "method": "session.event",
            "params": {"event": {"type": "assistant/attempt", "data": {"stream": [
                {"chunk": {"reason": {"kind": "error", "failure": {
                    "code": "TRANSPORT", "message": "DeepSeek Messages transport failed"
                }}}}
            ]}}}
        });
        assert_eq!(
            failure_detail(&attempt).as_deref(),
            Some("assistant/attempt: TRANSPORT DeepSeek Messages transport failed")
        );

        let turn_end = json!({
            "method": "session.event",
            "params": {"event": {"type": "turn/end", "data": {"reason": {"kind": "error",
                "failure": {"code": "RATE_LIMIT", "message": "quota exhausted"}}}}}
        });
        assert_eq!(
            failure_detail(&turn_end).as_deref(),
            Some("turn/end: RATE_LIMIT quota exhausted")
        );

        let legacy = json!({
            "method": "turn/end",
            "params": {"reason": {"kind": "error", "failure": {
                "code": "MODEL", "message": "bad request"
            }}}
        });
        assert_eq!(
            failure_detail(&legacy).as_deref(),
            Some("turn/end: MODEL bad request")
        );
    }

    #[test]
    fn failure_detail_is_bounded_and_ignores_unknown_shapes() {
        let long = "x".repeat(1_000);
        let frame = json!({
            "method": "session.event",
            "params": {"event": {"type": "turn/end", "data": {"reason": {"kind": "error",
                "failure": {"code": "LONG", "message": long}}}}}
        });
        let detail = failure_detail(&frame).expect("detail");
        assert_eq!(detail.chars().count(), FAILURE_DETAIL_MAX_CHARS + 1);
        assert!(detail.ends_with('…'));

        assert_eq!(
            failure_detail(&json!({"method": "turn/start", "params": {}})),
            None
        );
        assert_eq!(
            failure_detail(&json!({
                "method": "session.event",
                "params": {"event": {"type": "turn/end", "data": {"reason": {"kind": "error"}}}}
            })),
            None
        );
    }
}
