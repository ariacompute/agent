//! Audit seam: what actually happened, written at the moment it happened.
//!
//! The event list and field set are a stable schema (ADR-0021). Payloads are
//! **already redacted** by the time they reach a sink: no secret values, no raw
//! user content, no embedding vectors (repo rule 5).
//!
//! Audit is deliberately decoupled from the frozen streaming envelope
//! (`agent-cloud/src/event_envelope.rs`): adding audit fields there would change
//! the beta Agents contract.

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// The fixed event vocabulary. New kinds are additive; existing ones are not
/// renamed (consumers filter on these strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuditKind {
    /// An egress decision was made for a destination.
    EgressDecision,
    /// A credential placeholder was substituted at the boundary.
    CredentialSubstituted,
    /// A secret was created or rotated (metadata only).
    CredentialRotated,
    /// An egress policy snapshot was replaced or patched.
    PolicyChanged,
    /// A tool call completed (name, outcome, sizes — not the payload).
    ToolExecuted,
    /// A tool result was projected for the model (raw kept for audit).
    CompressionApplied,
    /// A sandbox lifecycle transition was requested or observed.
    LifecycleTransition,
    /// A quota reservation was granted or refused.
    QuotaDecision,
    /// Egress enforcement is off while traffic still flows (must be loud).
    UnenforcedEgress,
}

impl AuditKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            AuditKind::EgressDecision => "egress_decision",
            AuditKind::CredentialSubstituted => "credential_substituted",
            AuditKind::CredentialRotated => "credential_rotated",
            AuditKind::PolicyChanged => "policy_changed",
            AuditKind::ToolExecuted => "tool_executed",
            AuditKind::CompressionApplied => "compression_applied",
            AuditKind::LifecycleTransition => "lifecycle_transition",
            AuditKind::QuotaDecision => "quota_decision",
            AuditKind::UnenforcedEgress => "unenforced_egress",
        }
    }
}

/// One audit record. `metadata` must already be redacted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AuditEvent {
    pub id: String,
    pub ts_ms: u64,
    pub principal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    pub kind: AuditKind,
    /// Who/what the event is about (host, tool name, sandbox id, secret name).
    pub target: String,
    /// Present for decisions (`true` = allowed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<bool>,
    /// Egress policy revision in force when the event was written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub policy_revision: Option<u64>,
    /// Redacted, schema-stable extras.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub metadata: serde_json::Value,
}

impl AuditEvent {
    pub fn new(
        kind: AuditKind,
        principal_id: impl Into<String>,
        target: impl Into<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            ts_ms: now_ms(),
            principal_id: principal_id.into(),
            session_id: None,
            turn_id: None,
            kind,
            target: target.into(),
            decision: None,
            policy_revision: None,
            metadata: serde_json::Value::Null,
        }
    }

    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session_id = Some(session.into());
        self
    }

    pub fn with_turn(mut self, turn: impl Into<String>) -> Self {
        self.turn_id = Some(turn.into());
        self
    }

    pub fn with_decision(mut self, allowed: bool) -> Self {
        self.decision = Some(allowed);
        self
    }

    pub fn with_policy_revision(mut self, revision: u64) -> Self {
        self.policy_revision = Some(revision);
        self
    }

    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The seam. Sinks must be cheap on the hot path and must never block on a
/// remote round-trip: a slow audit sink would become a tool-latency problem.
#[async_trait]
pub trait AuditSink: Send + Sync {
    async fn record(&self, event: AuditEvent) -> SeamResult<()>;

    /// Most recent events, newest last. Deliveries are best-effort for
    /// `NoAudit`; real sinks must return what they persisted.
    async fn recent(&self, limit: usize) -> SeamResult<Vec<AuditEvent>>;
}

/// Discards events but counts them, so "audit is off" is observable rather than
/// invisible.
#[derive(Debug, Clone, Default)]
pub struct NoAudit {
    dropped: Arc<AtomicU64>,
}

impl NoAudit {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many events this sink swallowed.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl AuditSink for NoAudit {
    async fn record(&self, event: AuditEvent) -> SeamResult<()> {
        if event.kind == AuditKind::UnenforcedEgress {
            tracing::warn!(
                target = %event.target,
                "egress enforcement disabled; recording is no-op"
            );
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn recent(&self, _limit: usize) -> SeamResult<Vec<AuditEvent>> {
        Ok(Vec::new())
    }
}

/// A sink that refuses everything — for tests that assert "audit must be
/// configured". Not the default: an unconfigured deployment records nothing but
/// still runs (the `dropped` counter keeps that visible).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoAuditConfigured;

#[async_trait]
impl AuditSink for NoAuditConfigured {
    async fn record(&self, _event: AuditEvent) -> SeamResult<()> {
        Err(SeamError::Unavailable(
            "no audit sink is configured for this deployment".into(),
        ))
    }

    async fn recent(&self, _limit: usize) -> SeamResult<Vec<AuditEvent>> {
        Err(SeamError::Unavailable(
            "no audit sink is configured for this deployment".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_have_stable_wire_names() {
        assert_eq!(AuditKind::EgressDecision.as_str(), "egress_decision");
        assert_eq!(AuditKind::UnenforcedEgress.as_str(), "unenforced_egress");
        assert_eq!(
            AuditKind::CredentialSubstituted.as_str(),
            "credential_substituted"
        );
        assert_eq!(AuditKind::CredentialRotated.as_str(), "credential_rotated");
        assert_eq!(AuditKind::PolicyChanged.as_str(), "policy_changed");
        assert_eq!(AuditKind::ToolExecuted.as_str(), "tool_executed");
        assert_eq!(
            AuditKind::CompressionApplied.as_str(),
            "compression_applied"
        );
        assert_eq!(
            AuditKind::LifecycleTransition.as_str(),
            "lifecycle_transition"
        );
        assert_eq!(AuditKind::QuotaDecision.as_str(), "quota_decision");
        let json = serde_json::to_string(&AuditKind::QuotaDecision).unwrap();
        assert_eq!(json, "\"quota_decision\"");
        let back: AuditKind = serde_json::from_str("\"lifecycle_transition\"").unwrap();
        assert_eq!(back, AuditKind::LifecycleTransition);
    }

    #[test]
    fn builders_fill_the_schema_and_omit_empty_fields() {
        let event = AuditEvent::new(AuditKind::EgressDecision, "tenant-a", "api.openai.com")
            .with_session("s1")
            .with_turn("t1")
            .with_decision(false)
            .with_policy_revision(9)
            .with_metadata(serde_json::json!({"reason": "not on allowlist"}));
        assert_eq!(event.principal_id, "tenant-a");
        assert_eq!(event.session_id.as_deref(), Some("s1"));
        assert_eq!(event.turn_id.as_deref(), Some("t1"));
        assert_eq!(event.decision, Some(false));
        assert_eq!(event.policy_revision, Some(9));
        assert!(!event.id.is_empty());
        assert!(event.ts_ms > 0);

        let json = serde_json::to_string(&event).unwrap();
        assert!(json.contains("\"kind\":\"egress_decision\""));
        assert!(json.contains("api.openai.com"));
    }

    #[test]
    fn empty_optional_fields_are_skipped_on_the_wire() {
        let event = AuditEvent::new(AuditKind::ToolExecuted, "p", "web_fetch");
        let json = serde_json::to_string(&event).unwrap();
        assert!(!json.contains("session_id"));
        assert!(!json.contains("decision"));
        assert!(!json.contains("metadata"));
        let back: AuditEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, AuditKind::ToolExecuted);
    }

    #[tokio::test]
    async fn no_audit_counts_what_it_dropped() {
        let sink = NoAudit::new();
        assert_eq!(sink.dropped(), 0);
        sink.record(AuditEvent::new(AuditKind::ToolExecuted, "p", "t"))
            .await
            .unwrap();
        sink.record(AuditEvent::new(AuditKind::UnenforcedEgress, "p", "x"))
            .await
            .unwrap();
        assert_eq!(sink.dropped(), 2);
        assert!(sink.recent(10).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn unconfigured_sink_refuses_instead_of_pretending() {
        let sink = NoAuditConfigured;
        assert!(matches!(
            sink.record(AuditEvent::new(AuditKind::ToolExecuted, "p", "t"))
                .await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            sink.recent(1).await,
            Err(SeamError::Unavailable(_))
        ));
    }
}
