//! Cloud context storage: **Postgres + pgvector**.
//!
//! The contract ([`agent_core::context::ContextStore`]) lives in
//! `agent_core::context`; this module is the cloud implementation.
//! Conversational / long-term context therefore lives in Postgres (sharded per
//! tenant via `principal_id`) and semantic recall runs through pgvector's
//! cosine distance operator.
//!
//! pgvector is a **hard requirement**: [`ensure_pgvector`] fails startup when
//! the `vector` extension is missing. See
//! `docs/adr/0010-context-storage-pgvector.md`.

pub mod pg;

pub use pg::{ensure_pgvector, PgContextStore};
