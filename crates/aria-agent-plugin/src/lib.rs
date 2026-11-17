//! `aria-agent-plugin` — the capability-seam kernel.
//!
//! DeepSeek Harness ("everything is a plugin") splits every capability into one
//! of three roles: a **core** owner, a **seam** contract with swappable
//! providers, or a **bundle** that only composes other seams. This crate is the
//! Rust equivalent, without Cordis's runtime DI (see
//! `docs/adr/0014-plugin-capability-seams.md`):
//!
//! ```text
//! PluginManifest ──► Registry (builder) ──► FrozenRegistry (immutable)
//!                        │                        │
//!                   provide(seam, name, Arc)   resolve::<T>(seam)
//! ```
//!
//! Assembly is fail-closed: a missing dependency, a dependency cycle, a
//! duplicate provider name, or two owners for a core seam aborts startup instead
//! of silently degrading a request.

pub mod error;
pub mod manifest;
pub mod registry;

pub use error::{PluginError, PluginResult};
pub use manifest::{
    PluginManifest, SeamKey, SeamRole, AGENTS, AGENT_LOOP, ALL, APPROVALS, BROWSER_USE, COMPACTION,
    FS, JOBS, LLM, SANDBOX, SANDBOX_POLICY, SCHEDULE, SESSION, SESSION_PERSISTENCE, SESSION_QUERY,
    SESSION_TITLE, SHELL, SKILLS, STORAGE, SUBAGENTS, SYSTEM_PROMPT, TEAMS, TERMINAL,
    USER_QUESTIONS, WEB, WORKING_DIRECTORY,
};
pub use registry::{FrozenRegistry, Plugin, Provider, Registry, Shared};
