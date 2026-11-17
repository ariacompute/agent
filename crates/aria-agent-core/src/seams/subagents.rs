//! Sub-agent seam (`ctx.subagents`).
//!
//! dsh ships several providers (in-process spawn, fork, ACP, external CLIs) and
//! keeps one activation manager owning both lifecycles. The Rust port keeps the
//! contract: a parent asks for a child, gets an id + session, and can enumerate
//! its children. Isolation boundaries (same store? same sandbox?) belong to the
//! provider, not to the tool (ADR-0017).

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Separator between a parent session and one of its children.
///
/// Lives here (not in a provider crate) so the loop, every provider and the
/// cloud agree on how a child session id is spelled and can walk back up to the
/// parent that owns it — that is what makes a child's title addressable
/// (`GET /v1/agents/sessions/<child>/title`) without persisting the child as a
/// first-class session (ADR-0017).
pub const CHILD_SESSION_SEP: &str = "::sub::";

/// `<parent>::sub::<id>` — the child's own session id.
pub fn child_session(parent: &str, id: &str) -> String {
    format!("{parent}{CHILD_SESSION_SEP}{id}")
}

/// The immediate parent of a child session, or `None` for a root session.
///
/// Walks exactly one level: `a::sub::b::sub::c` reports `a::sub::b`, so nested
/// delegation stays addressable level by level.
pub fn parent_session(session: &str) -> Option<&str> {
    let (parent, _child) = session.rsplit_once(CHILD_SESSION_SEP)?;
    if parent.is_empty() {
        return None;
    }
    Some(parent)
}

/// Whether `session` names a child (delegated) session.
pub fn is_child_session(session: &str) -> bool {
    parent_session(session).is_some()
}

/// Request to run one child agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentRequest {
    /// Session that owns (and can observe) the child.
    pub parent_session: String,
    /// Self-contained task handed to the child.
    pub task: String,
    /// Optional agent name override (defaults to `<parent>-child`).
    #[serde(default)]
    pub agent_name: Option<String>,
    /// Hard step cap for the child (0 = provider default).
    #[serde(default)]
    pub max_steps: usize,
}

impl SubagentRequest {
    pub fn new(parent_session: impl Into<String>, task: impl Into<String>) -> Self {
        Self {
            parent_session: parent_session.into(),
            task: task.into(),
            agent_name: None,
            max_steps: 0,
        }
    }
}

/// Handle to a spawned child.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentHandle {
    pub id: String,
    pub session: String,
    pub parent_session: String,
}

#[async_trait]
pub trait Subagents: Send + Sync {
    async fn spawn(&self, req: SubagentRequest) -> SeamResult<SubagentHandle>;
    async fn list(&self, parent_session: &str) -> SeamResult<Vec<SubagentHandle>>;

    /// Wait for a child to finish and return its final text.
    ///
    /// Providers that only *start* children (an out-of-process CLI, a queue) may
    /// leave this unimplemented; the parent then reads the child's session
    /// instead. Failures are reported, never silently empty.
    async fn wait(&self, _id: &str) -> SeamResult<String> {
        Err(SeamError::Unsupported(
            "this subagent provider cannot report child output".into(),
        ))
    }
}

/// Fail-closed default: delegation is not configured.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSubagents;

#[async_trait]
impl Subagents for NoSubagents {
    async fn spawn(&self, _req: SubagentRequest) -> SeamResult<SubagentHandle> {
        Err(SeamError::Unavailable(
            "no subagent provider is configured for this deployment".into(),
        ))
    }

    async fn list(&self, _parent_session: &str) -> SeamResult<Vec<SubagentHandle>> {
        Ok(Vec::new())
    }

    async fn wait(&self, _id: &str) -> SeamResult<String> {
        Err(SeamError::Unavailable(
            "no subagent provider is configured for this deployment".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_subagents_fails_closed_but_lists_empty() {
        let s = NoSubagents;
        assert!(matches!(
            s.spawn(SubagentRequest::new("p", "do it")).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(s.list("p").await.unwrap().is_empty());
    }

    #[test]
    fn child_sessions_round_trip_to_their_parent() {
        let child = child_session("sess-1", "abc");
        assert_eq!(child, "sess-1::sub::abc");
        assert_eq!(parent_session(&child), Some("sess-1"));
        assert!(is_child_session(&child));
        // A root session has no parent, and the separator alone is not a parent.
        assert_eq!(parent_session("sess-1"), None);
        assert!(!is_child_session("sess-1"));
        assert_eq!(parent_session("::sub::abc"), None);
        // One level only: the immediate parent of a grandchild is another child.
        let grandchild = child_session(&child, "def");
        assert_eq!(parent_session(&grandchild), Some(child.as_str()));
        assert_eq!(
            parent_session(parent_session(&grandchild).unwrap()),
            Some("sess-1")
        );
    }

    #[test]
    fn request_roundtrips() {
        let req = SubagentRequest::new("parent", "task");
        let json = serde_json::to_string(&req).unwrap();
        let back: SubagentRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(back, req);
        assert_eq!(back.max_steps, 0);
        assert!(back.agent_name.is_none());
    }
}
