//! Server-sent-event encoding for A2A streams.
//!
//! There is no sentinel frame: the stream ends after the `final: true` envelope
//! (or on `input-required`, which is also final). A client that wants to know
//! "done" reads `final` rather than looking for `[DONE]`.

use crate::types::StreamFrame;

/// The SSE `event:` name for a frame kind.
///
/// `task` is the fallback for an unknown kind; the only callers pass one of the
/// four A2A kinds.
pub fn event_name(kind: &str) -> &'static str {
    match kind {
        "status-update" => "task_status_update",
        "artifact-update" => "task_artifact_update",
        "message" => "message",
        _ => "task",
    }
}

/// One SSE block: `event:` line, one `data:` line, blank line terminator.
pub fn sse_frame(event: &str, data: &str) -> String {
    format!("event: {event}\ndata: {data}\n\n")
}

/// Encode a frame as it goes on the wire.
///
/// Serialization cannot fail for these types (no non-string map keys and no
/// non-finite numbers can reach `serde_json`), so the expect is structural, not
/// a swallowed error.
pub fn encode(frame: &StreamFrame) -> String {
    let data = serde_json::to_string(frame).expect("a2a frames serialize to JSON");
    sse_frame(frame.event(), &data)
}
