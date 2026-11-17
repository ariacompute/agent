//! One-shot approval decisions.
//!
//! dsh's `ctx.approvals` is a **seam**: whoever can answer (a UI, an ACP bridge)
//! registers a provider, and with **no** provider the decision is
//! `Unavailable` — which the tool pipeline treats as a **denial**. Silent
//! auto-allow is never the fallback (ADR-0016 §fail-closed).

use super::policy::PermissionPreset;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A single approval question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub tool: String,
    pub session: String,
    pub arguments: Value,
    pub preset: PermissionPreset,
}

impl ApprovalRequest {
    pub fn new(tool: impl Into<String>, session: impl Into<String>, arguments: Value) -> Self {
        Self {
            tool: tool.into(),
            session: session.into(),
            arguments,
            preset: PermissionPreset::default(),
        }
    }

    pub fn with_preset(mut self, preset: PermissionPreset) -> Self {
        self.preset = preset;
        self
    }
}

/// The answer. `Unavailable` means "nobody could answer", not "allowed".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny { reason: String },
    Unavailable { reason: String },
}

impl ApprovalDecision {
    /// Only `Allow` permits execution.
    pub fn is_allowed(&self) -> bool {
        matches!(self, ApprovalDecision::Allow)
    }

    /// Human-readable reason for the tool result fed back to the model.
    pub fn reason(&self) -> String {
        match self {
            ApprovalDecision::Allow => "approved".into(),
            ApprovalDecision::Deny { reason } => format!("denied: {reason}"),
            ApprovalDecision::Unavailable { reason } => format!("approval unavailable: {reason}"),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ApprovalDecision::Allow => "allow",
            ApprovalDecision::Deny { .. } => "deny",
            ApprovalDecision::Unavailable { .. } => "unavailable",
        }
    }
}

/// Pluggable approval answering.
#[async_trait]
pub trait ApprovalBroker: Send + Sync {
    async fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision;
}

/// Fail-closed default: there is no human channel, so every mutating call is
/// refused with an actionable reason.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

#[async_trait]
impl ApprovalBroker for DenyAll {
    async fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Unavailable {
            reason: format!(
                "no approval provider is configured for `{}`; \
                 set `approval_mode = \"auto\"` or select `danger_full_access`",
                req.tool
            ),
        }
    }
}

/// Trusted deployment setting: approve without asking.
///
/// Only ever installed explicitly (tests, `danger_full_access`); it is never the
/// fallback when a broker is missing.
#[derive(Debug, Clone, Copy, Default)]
pub struct AutoAllow;

#[async_trait]
impl ApprovalBroker for AutoAllow {
    async fn decide(&self, _req: &ApprovalRequest) -> ApprovalDecision {
        ApprovalDecision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn deny_all_is_fail_closed() {
        let broker = DenyAll;
        let req = ApprovalRequest::new("shell", "s1", json!({"command": ["ls"]}));
        let decision = broker.decide(&req).await;
        assert!(!decision.is_allowed());
        assert!(matches!(decision, ApprovalDecision::Unavailable { .. }));
        assert_eq!(decision.as_str(), "unavailable");
        assert!(decision.reason().contains("no approval provider"));
    }

    #[tokio::test]
    async fn auto_allow_allows() {
        let broker = AutoAllow;
        let req = ApprovalRequest::new("fs_write", "s1", json!({}));
        assert!(broker.decide(&req).await.is_allowed());
    }

    #[test]
    fn decisions_serialize_with_tag() {
        let d = ApprovalDecision::Deny {
            reason: "nope".into(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["decision"], "deny");
        assert_eq!(v["reason"], "nope");
        let back: ApprovalDecision = serde_json::from_value(v).unwrap();
        assert_eq!(back, d);
    }

    #[test]
    fn request_defaults_to_workspace_write() {
        let req = ApprovalRequest::new("shell", "s", json!(null));
        assert_eq!(req.preset, PermissionPreset::WorkspaceWrite);
        let req = req.with_preset(PermissionPreset::FullAccess);
        assert_eq!(req.preset, PermissionPreset::FullAccess);
    }
}
