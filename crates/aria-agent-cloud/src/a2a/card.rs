//! The public Agent Card projection.
//!
//! Two sources, in this order: the deployment's `A2A_CARDS_FILE` (description
//! and skills) and a neutral default. Nothing is derived from the agent's
//! instructions or its native tool families — the default dsh harness runs its
//! own tools inside a sandbox container, so advertising `shell_*`/`fs_*` as
//! A2A capabilities would be a lie, and instructions are not public.
//!
//! The card is built per request from the agent row (id/name) so a renamed agent
//! is reflected without a restart, and it never carries a principal, a
//! credential or a sandbox volume/container name.

use aria_agent_a2a::card::{AgentCard, AgentSkill};
use serde::Deserialize;

/// `A2A_CARDS_FILE` contents.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CardOverrides {
    /// Replaces the neutral `"{name} (A2A agent)"` description.
    #[serde(default)]
    pub description: Option<String>,
    /// Replaces the single neutral `chat` skill.
    #[serde(default)]
    pub skills: Option<Vec<AgentSkill>>,
}

impl CardOverrides {
    /// Parse the deployment file. A malformed file is a boot error, not a
    /// silently ignored one.
    pub fn from_json(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|e| format!("A2A_CARDS_FILE: {e}"))
    }
}

/// Build one agent's card.
///
/// `path` is the agent's JSON-RPC path on this server (`/a2a` for the default
/// agent, `/v1/agents/<id>/a2a` otherwise).
pub fn build_card(
    public_url: &str,
    agent_name: &str,
    path: &str,
    overrides: &CardOverrides,
) -> AgentCard {
    let description = overrides
        .description
        .clone()
        .unwrap_or_else(|| format!("{agent_name} (A2A agent)"));
    let mut card = AgentCard::for_agent(
        agent_name,
        description,
        AgentCard::endpoint(public_url, path),
        env!("CARGO_PKG_VERSION"),
    );
    if let Some(skills) = &overrides.skills {
        card.skills = skills.clone();
    }
    card
}
