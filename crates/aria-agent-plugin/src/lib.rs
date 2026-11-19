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
    PluginManifest, SeamKey, SeamRole, AGENTS, AGENT_LOOP, ALIASES, ALL, APPROVALS, ATTACHMENTS,
    AUTHORIZATION, BROWSER_USE, COMPACTION, COMPUTER_USE, CONNECTION_AUTHENTICATION, CREDENTIALS,
    DEEPSEEK_AUTH, DEEPSEEK_LLM_API_EXTENSIONS, DIRECTORY_PICKER, FILE_REFERENCES, FORGE, FS, JOBS,
    LLM, LSP, MCP_RESOURCES, OTEL, PLUGIN_MANAGER, PRODUCT_TELEMETRY, PTC_RUNTIME, SANDBOX,
    SANDBOX_POLICY, SCHEDULE, SESSION, SESSION_PERSISTENCE, SESSION_QUERY, SESSION_TELEMETRY,
    SESSION_TITLE, SHELL, SKILLS, SPEECH_TO_TEXT, SPILL_STORE, STORAGE, SUBAGENTS, SUBPROCESS,
    SYSTEM_PROMPT, TEAMS, TERMINAL, TOKEN_METER, TOOLS, USER_QUESTIONS, WEB, WORKFLOW_ENGINE,
    WORKING_DIRECTORY, WORKTREES,
};
pub use registry::{FrozenRegistry, Plugin, Provider, Registry, Shared};
