//! Session seams (`ctx.sessions`, `ctx.sessionPersistence`, `ctx.sessionQuery`,
//! `ctx.sessionTitle`, `ctx.compaction`).
//!
//! The contracts live in [`crate::session`] because the event vocabulary is the
//! single source of truth; this module is the seam-indexed re-export so a plugin
//! manifest and a consumer refer to the same names (ADR-0014).

pub use crate::session::{
    first_prompt_title, query, search_text, CompactDecision, CompactReason, CompactionPolicy,
    JsonlPersistence, SessionError, SessionEvent, SessionFilter, SessionPersistence, SessionRecord,
    SessionResult, SessionTitleProvider,
};

/// Provider name for the built-in JSONL backend.
pub const JSONL_PROVIDER: &str = "jsonl";
