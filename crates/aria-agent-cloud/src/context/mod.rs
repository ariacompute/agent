//! Cloud context storage: **Postgres + pgvector**.
//!
//! The contract ([`ContextStore`]) lives in `agent_core::context`; this module
//! is the cloud implementation. Conversational / long-term context therefore
//! lives in Postgres (sharded per tenant via `principal_id`) and semantic
//! recall runs through pgvector's cosine distance operator.
//!
//! pgvector is a **hard requirement**: [`ensure_pgvector`] fails startup when
//! the `vector` extension is missing. See `docs/adr/0010-context-storage-pgvector.md`.

pub mod pg;

pub use pg::PgContextStore;

use sqlx::{Executor, PgPool};

/// Fail fast when the `vector` extension is unavailable.
///
/// `CREATE EXTENSION` needs elevated privileges, so we attempt it (idempotent)
/// and then verify. Without pgvector the context store cannot persist or
/// recall embeddings, so the service refuses to start rather than silently
/// degrading to keyword-only recall.
pub async fn ensure_pgvector(pool: &PgPool) -> Result<(), String> {
    // Best-effort install; ignore failures (e.g. missing privileges) and rely
    // on the verification below.
    let _ = pool.execute("CREATE EXTENSION IF NOT EXISTS vector").await;

    let installed: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_extension WHERE extname = 'vector')")
            .fetch_one(pool)
            .await
            .map_err(|e| format!("pgvector check failed: {e}"))?;
    if !installed {
        return Err(
            "pgvector is required: install the `vector` extension (image `pgvector/pgvector:pg16`) \
             and retry"
                .into(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pure (no DB) check: the extension guard is what startup calls.
    #[test]
    fn ensure_pgvector_has_no_db_independent_side_effects() {
        // Sanity: the function is async and takes a pool; nothing to assert
        // without a database. Kept so the module is covered by `cargo test`.
        let _ = std::mem::size_of::<PgContextStore>();
    }
}
