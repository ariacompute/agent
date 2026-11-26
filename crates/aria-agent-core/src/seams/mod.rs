//! Capability seams.
//!
//! Each sub-module is one *contract* a provider plugs into (dsh's `ctx.*`
//! services). Seams expose the minimum a consumer needs and no escape hatch:
//! `web` only searches/fetches, `fs` only touches paths under the single
//! [`SandboxPolicy`] root, `approvals` only answers allow/deny (ADR-0014 §契约原则).
//!
//! `aria-agent-core` keeps depending on traits only — never on `aria-agent-memo`,
//! `aria-agent-tools` or a concrete sandbox.

pub mod agent_loop;
pub mod approvals;
pub mod audit;
pub mod credentials;
pub mod egress;
pub mod fs;
pub mod jobs;
pub mod llm;
pub mod metering;
pub mod policy;
pub mod questions;
pub mod rsi;
pub mod schedule;
pub mod session;
pub mod shell;
pub mod skills;
pub mod storage;
pub mod subagents;
pub mod tools;
pub mod web;

use thiserror::Error;

pub use agent_loop::{AgentLoop, LoopContext};
pub use approvals::{ApprovalBroker, ApprovalDecision, ApprovalRequest, AutoAllow, DenyAll};
pub use audit::{now_ms, AuditEvent, AuditKind, AuditSink, NoAudit, NoAuditConfigured};
pub use credentials::{
    CredentialsSeam, NoCredentials, SecretMeta, SecretStub, SecretValue, SubstitutionReport,
    CREDENTIAL_PLACEHOLDER,
};
pub use egress::{
    DenyAllEgress, EgressDecision, EgressMode, EgressPolicy, EgressSeam, HostPattern,
    MAX_HOST_PATTERN_LEN,
};
pub use fs::{FileStat, FsSeam, LocalFs};
pub use jobs::{JobId, JobRecord, JobState, JobsSeam, LocalJobs};
pub use llm::TokenUsage;
pub use metering::{
    BilledTokens, EstimatedTokens, MeterState, MeteringSeam, NoMetering, NoMeteringConfigured,
    ResourceRequest, ResourceUsage, UsageRecord, UsageSummary,
};
pub use policy::{PermissionPreset, PolicyError, SandboxPolicy};
pub use questions::{NoQuestions, UserQuestions};
pub use rsi::{
    Adaptation, NoPlaybooks, NoReflection, Playbook, PlaybookCandidate, PlaybookStore, Proposal,
    ReflectionEngine, ReplayStats, RerunProbe, RsiConfig, RsiScope, ValidationOutcome,
};
pub use schedule::{ScheduleEntry, ScheduleSeam};
pub use shell::{SandboxShell, ShellOutput, ShellSeam};
pub use skills::{EmptySkills, FilesystemSkills, Skill, SkillMeta, SkillsSeam};
pub use storage::Storage;
pub use subagents::{
    child_session, is_child_session, parent_session, NoSubagents, SubagentHandle, SubagentRequest,
    Subagents, CHILD_SESSION_SEP,
};
pub use tools::{
    Authz, NoProjection, PostExecPolicy, Projection, ProjectionRecord, SpillRef,
    ToolExecutionPipeline, ToolOutcome, ToolOutputProjector,
};
pub use web::{is_public_url, NoWeb, WebResult, WebSeam};

/// Failure of a capability provider.
///
/// `Unavailable` is the fail-closed default: a deployment that did not configure
/// a provider must degrade to an actionable error, never to a silent success.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum SeamError {
    #[error("capability unavailable: {0}")]
    Unavailable(String),
    #[error("denied: {0}")]
    Denied(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("io error: {0}")]
    Io(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// A revision guard refused the write (the caller's view of "current" is
    /// stale). Never resolved by last-writer-wins.
    #[error("conflict: {0}")]
    Conflict(String),
    /// Cross-tenant reuse without a complete [`rsi::Adaptation`] record.
    #[error("adaptation required: {0}")]
    Unadapted(String),
}

impl SeamError {
    /// Stable error code used by tools when feeding the failure back to the model.
    ///
    /// `conflict` and `unadapted` are part of the public machine-readable
    /// contract (ADR-0026): the cloud maps them to `409` and `422`, and audit
    /// records keep them verbatim.
    pub fn code(&self) -> &'static str {
        match self {
            SeamError::Unavailable(_) => "unavailable",
            SeamError::Denied(_) => "denied",
            SeamError::NotFound(_) => "not_found",
            SeamError::Invalid(_) => "invalid",
            SeamError::Io(_) => "io",
            SeamError::Unsupported(_) => "unsupported",
            SeamError::Conflict(_) => "conflict",
            SeamError::Unadapted(_) => "unadapted",
        }
    }
}

/// Result alias for seam calls.
pub type SeamResult<T> = Result<T, SeamError>;
#[cfg(test)]
mod error_code_tests {
    use super::SeamError;

    /// The codes are a wire contract (tool failure text, HTTP mapping, audit
    /// records): renaming one silently would break every consumer.
    #[test]
    fn every_error_has_a_stable_machine_readable_code() {
        let cases = [
            (SeamError::Unavailable("x".into()), "unavailable"),
            (SeamError::Denied("x".into()), "denied"),
            (SeamError::NotFound("x".into()), "not_found"),
            (SeamError::Invalid("x".into()), "invalid"),
            (SeamError::Io("x".into()), "io"),
            (SeamError::Unsupported("x".into()), "unsupported"),
            (SeamError::Conflict("x".into()), "conflict"),
            (SeamError::Unadapted("x".into()), "unadapted"),
        ];
        for (error, code) in cases {
            assert_eq!(error.code(), code, "{}", error);
        }
    }
}
