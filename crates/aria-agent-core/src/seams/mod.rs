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
pub mod fs;
pub mod jobs;
pub mod llm;
pub mod policy;
pub mod questions;
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
pub use fs::{FileStat, FsSeam, LocalFs};
pub use jobs::{JobId, JobRecord, JobState, JobsSeam, LocalJobs};
pub use llm::TokenUsage;
pub use policy::{PermissionPreset, PolicyError, SandboxPolicy};
pub use questions::{NoQuestions, UserQuestions};
pub use schedule::{ScheduleEntry, ScheduleSeam};
pub use shell::{SandboxShell, ShellOutput, ShellSeam};
pub use skills::{EmptySkills, FilesystemSkills, Skill, SkillMeta, SkillsSeam};
pub use storage::Storage;
pub use subagents::{
    child_session, is_child_session, parent_session, NoSubagents, SubagentHandle, SubagentRequest,
    Subagents, CHILD_SESSION_SEP,
};
pub use tools::{Authz, PostExecPolicy, SpillRef, ToolExecutionPipeline, ToolOutcome};
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
}

impl SeamError {
    /// Stable error code used by tools when feeding the failure back to the model.
    pub fn code(&self) -> &'static str {
        match self {
            SeamError::Unavailable(_) => "unavailable",
            SeamError::Denied(_) => "denied",
            SeamError::NotFound(_) => "not_found",
            SeamError::Invalid(_) => "invalid",
            SeamError::Io(_) => "io",
            SeamError::Unsupported(_) => "unsupported",
        }
    }
}

/// Result alias for seam calls.
pub type SeamResult<T> = Result<T, SeamError>;
