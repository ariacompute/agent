//! Storage seam.
//!
//! The typed domain contract lives in [`crate::context`] (`ContextStore`). dsh
//! splits "opaque KV primitives" (`ctx.storage`) from "typed domain operations"
//! (`ctx.storageDomain`); here the typed domain *is* the seam, because every
//! backend we ship (SQLite memo, Postgres + pgvector) wants typed operations and
//! an opaque KV cell would only add a translation layer.
//!
//! `aria-agent-core` depends on this trait and never on a backend crate
//! (AGENTS.md rule 13).

pub use crate::context::ContextStore as Storage;

/// Provider name used when a single store backs both context and sessions.
pub const DEFAULT_PROVIDER: &str = "default";
