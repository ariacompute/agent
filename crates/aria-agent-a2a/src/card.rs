//! The discovery document served at `/.well-known/agent-card.json`.
//!
//! The card is **public**, so it is a projection, not a dump: it never carries
//! the agent's instructions, tool arguments, principal, credentials or the
//! sandbox volume/container name. Skills may be replaced by the deployment
//! through `A2A_CARDS_FILE`; when they are not, a single neutral `chat` skill is
//! advertised rather than deriving one per native tool family (the default dsh
//! harness runs its own tools inside a container, so those families are not
//! capabilities an A2A caller can rely on).

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::types::Authentication;

/// The A2A protocol version this server implements.
pub const PROTOCOL_VERSION: &str = "1.0.0";

/// The discovery document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCard {
    #[serde(default = "protocol_version")]
    pub protocol_version: String,
    pub name: String,
    pub description: String,
    /// The JSON-RPC endpoint for this agent.
    pub url: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub default_input_modes: Vec<String>,
    #[serde(default)]
    pub default_output_modes: Vec<String>,
    #[serde(default)]
    pub skills: Vec<AgentSkill>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<Authentication>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<Provider>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub documentation_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

fn protocol_version() -> String {
    PROTOCOL_VERSION.to_string()
}

impl AgentCard {
    /// A card advertising streaming, push and history with one neutral `chat`
    /// skill. Callers override `description` / `skills` from their own config.
    pub fn for_agent(
        name: impl Into<String>,
        description: impl Into<String>,
        url: impl Into<String>,
        version: impl Into<String>,
    ) -> Self {
        let name = name.into();
        Self {
            protocol_version: protocol_version(),
            description: description.into(),
            url: url.into(),
            version: version.into(),
            capabilities: Capabilities {
                streaming: true,
                push_notifications: true,
                state_transition_history: true,
            },
            default_input_modes: vec!["text".to_string()],
            default_output_modes: vec!["text".to_string(), "data".to_string()],
            skills: vec![AgentSkill {
                id: "chat".to_string(),
                description: "Conversational agent".to_string(),
                name: name.clone(),
                ..AgentSkill::default()
            }],
            authentication: Some(Authentication {
                schemes: vec!["bearer".to_string()],
                credentials: None,
            }),
            provider: Some(Provider {
                organization: "ariacompute".to_string(),
                url: None,
            }),
            name,
            documentation_url: None,
            metadata: None,
        }
    }

    /// Join `base` and `path` into the card's endpoint URL, tolerating a
    /// trailing slash on the base and a missing/leading slash on the path.
    pub fn endpoint(base: &str, path: &str) -> String {
        let base = base.trim_end_matches('/');
        let path = path.trim_start_matches('/');
        if path.is_empty() {
            base.to_string()
        } else {
            format!("{base}/{path}")
        }
    }
}

/// Optional A2A features this server enables.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Capabilities {
    #[serde(default)]
    pub streaming: bool,
    #[serde(default)]
    pub push_notifications: bool,
    #[serde(default)]
    pub state_transition_history: bool,
}

/// One advertised capability. Only the deployment's config describes these.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentSkill {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resource_kind: Option<String>,
}

/// Who operates the agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Provider {
    pub organization: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}
