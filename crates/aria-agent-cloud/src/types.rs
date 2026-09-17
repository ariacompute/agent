//! Wire types for the **OpenAI beta Agents** resource surface
//! (`/v1/agents`, `/v1/agents/sessions`, …).
//!
//! Field names mirror the published reference so an OpenAI client can consume
//! this service unchanged. Fields we do not implement yet are either omitted
//! (they are optional in the reference) or served by the 501 stubs in
//! [`crate::api::stubs`].

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const AGENT_OBJECT: &str = "agent";
pub const SESSION_OBJECT: &str = "agent.session";
pub const TURN_OBJECT: &str = "agent.session.turn";
pub const ITEM_OBJECT: &str = "agent.session.item";
pub const SUBAGENT_OBJECT: &str = "agent.session.subagent";
pub const LIST_OBJECT: &str = "list";

/// Amount of reasoning effort used by an agent.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentReasoning {
    /// `none` | `minimal` | `low` | `medium` | `high` | `xhigh` | `max`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// `concise` | `detailed` | `auto`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
}

/// Configuration for creating and coordinating subagents.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MultiAgentConfig {
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_subagents: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentText {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verbosity: Option<String>,
}

/// A reusable agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Agent {
    pub id: String,
    pub object: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Custom instructions appended to the agent's default base instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub reasoning: AgentReasoning,
    pub multi_agent: MultiAgentConfig,
    pub service_tier: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<AgentText>,
    pub tools: Vec<Value>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateAgentRequest {
    pub name: Option<String>,
    pub model: Option<String>,
    pub instructions: Option<String>,
    pub reasoning: Option<AgentReasoning>,
    pub multi_agent: Option<MultiAgentConfig>,
    pub service_tier: Option<String>,
    pub text: Option<AgentText>,
    pub tools: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateAgentRequest {
    pub name: Option<String>,
    pub model: Option<String>,
    pub instructions: Option<String>,
    pub reasoning: Option<AgentReasoning>,
    pub multi_agent: Option<MultiAgentConfig>,
    pub service_tier: Option<String>,
    pub text: Option<AgentText>,
    pub tools: Option<Vec<Value>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AgentDeleted {
    pub id: String,
    pub object: String,
    pub deleted: bool,
}

/// The agent bound to a session (a snapshot of the reusable agent).
#[derive(Debug, Clone, Serialize)]
pub struct SessionAgent {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub reasoning: AgentReasoning,
    pub multi_agent: MultiAgentConfig,
    pub service_tier: String,
}

/// A session: a stateful conversation with an agent.
#[derive(Debug, Clone, Serialize)]
pub struct AgentSession {
    pub id: String,
    pub object: String,
    pub agent: SessionAgent,
    /// Session-level instructions appended to the agent's own instructions.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    pub status: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct CreateSessionRequest {
    /// Agent id (preferred) — falls back to `agent` (id or human-friendly name).
    pub agent_id: Option<String>,
    pub agent: Option<String>,
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct UpdateSessionRequest {
    pub instructions: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SessionDeleted {
    pub id: String,
    pub object: String,
    pub deleted: bool,
}

/// One turn of a session (a user input plus the agent's reply).
#[derive(Debug, Clone, Serialize)]
pub struct SessionTurn {
    pub id: String,
    pub object: String,
    pub session_id: String,
    pub agent_id: String,
    /// `in_progress` | `completed` | `failed` | `cancelled`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<i64>,
}

/// An item produced inside a turn (message / function call / tool output).
#[derive(Debug, Clone, Serialize)]
pub struct SessionItem {
    pub id: String,
    pub object: String,
    pub session_id: String,
    pub turn_id: String,
    /// `message` | `function_call` | `function_call_output`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `user` | `assistant` | `system` — only for message items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub status: String,
    /// Message content (string or content-part array) / tool output text.
    pub content: Value,
    pub created_at: i64,
}

/// A subagent spawned inside a session. This runtime runs a single agent loop,
/// so subagent listings are always empty; the shape is kept so clients that
/// enumerate subagents do not break.
#[derive(Debug, Clone, Serialize)]
pub struct Subagent {
    pub id: String,
    pub object: String,
    pub name: String,
    pub status: String,
}

/// The OpenAI-style list envelope.
#[derive(Debug, Clone, Serialize)]
pub struct ListResponse<T: Serialize> {
    pub object: String,
    pub data: Vec<T>,
    pub has_more: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_id: Option<String>,
}

impl<T: Serialize> ListResponse<T> {
    pub fn new(data: Vec<T>) -> Self {
        Self {
            object: LIST_OBJECT.to_string(),
            has_more: false,
            first_id: None,
            last_id: None,
            data,
        }
    }
}

/// One memory fragment stored in the session's context store.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `message` | `tool_result` | `long_term` | `note`.
    pub kind: String,
    pub content: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PutMemoryRequest {
    /// Omit for an unkeyed context fragment; set for a long-term memory.
    #[serde(default)]
    pub key: Option<String>,
    pub value: String,
    /// `message` | `tool_result` | `long_term` (default) | `note`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// A user input event posted to `POST /v1/agents/sessions/{id}/events`.
///
/// Accepts either a plain string or the OpenAI input-content shape
/// (`{"type":"input_text","text":"..."}`), plus an optional array form.
#[derive(Debug, Clone, Deserialize)]
pub struct SessionInputEvent {
    /// Plain-text input (short form).
    pub input: Option<String>,
    /// Structured input parts (OpenAI shape).
    #[serde(default)]
    pub content: Vec<InputContent>,
    /// Optional type tag; defaults to `message`.
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct InputContent {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    pub kind: Option<String>,
    pub text: Option<String>,
}

impl SessionInputEvent {
    /// Collapse the accepted shapes into the text handed to the agent.
    pub fn text(&self) -> String {
        if let Some(t) = self.input.as_deref() {
            return t.to_string();
        }
        self.content
            .iter()
            .filter_map(|c| c.text.as_deref())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_event_reads_plain_string() {
        let e: SessionInputEvent = serde_json::from_str(r#"{"input":"hello"}"#).unwrap();
        assert_eq!(e.text(), "hello");
    }

    #[test]
    fn input_event_reads_content_parts() {
        let e: SessionInputEvent =
            serde_json::from_str(r#"{"content":[{"type":"input_text","text":"hi"}]}"#).unwrap();
        assert_eq!(e.text(), "hi");
    }

    #[test]
    fn input_event_missing_both_is_empty() {
        let e: SessionInputEvent = serde_json::from_str(r#"{"type":"message"}"#).unwrap();
        assert_eq!(e.text(), "");
    }

    #[test]
    fn agent_serializes_openai_shape() {
        let a = Agent {
            id: "agent_1".into(),
            object: AGENT_OBJECT.into(),
            name: Some("Demo".into()),
            model: Some("gpt-4o-mini".into()),
            instructions: Some("be brief".into()),
            reasoning: AgentReasoning::default(),
            multi_agent: MultiAgentConfig::default(),
            service_tier: "auto".into(),
            text: None,
            tools: vec![],
            created_at: 0,
        };
        let v = serde_json::to_value(&a).unwrap();
        assert_eq!(v["object"], serde_json::json!("agent"));
        assert_eq!(v["service_tier"], serde_json::json!("auto"));
        assert!(v["multi_agent"]["enabled"].is_boolean());
    }

    #[test]
    fn list_response_is_openai_shaped() {
        let l = ListResponse::new(vec!["a".to_string()]);
        let v = serde_json::to_value(&l).unwrap();
        assert_eq!(v["object"], serde_json::json!("list"));
        assert_eq!(v["data"][0], serde_json::json!("a"));
        assert_eq!(v["has_more"], serde_json::json!(false));
    }
}
