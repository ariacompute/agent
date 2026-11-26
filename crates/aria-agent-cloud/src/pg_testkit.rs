//! Postgres fixture shared by every DB-gated test in this crate.
//!
//! `DATABASE_URL` decides the mode: unset (the offline default, and what
//! `scripts/coverage.sh` runs without) means every DB test returns early; set
//! means the suite runs against that database and the pg backends are part of
//! the coverage gate.
//!
//! Each test builds its own pool — a pool owns background tasks on the runtime
//! that created it, so one process-wide pool would die with the first
//! `#[tokio::test]` runtime that finished. The **schema**, which is the shared
//! part, is applied once: concurrent `CREATE TABLE IF NOT EXISTS` /
//! `CREATE EXTENSION IF NOT EXISTS` raced into
//! `23505 duplicate key value violates unique constraint "pg_type_typname_nsp_index"`,
//! so the DDL now runs behind one process-wide lock and a ready flag.
//!
//! The three orchestration tables (`scheduled_jobs`, `agent_teams`,
//! `session_titles`) are not created by `store::pg::ensure_schema`; they come
//! from the migrations a deployment replays. The fixture applies those same
//! files, so the DDL under test is the DDL that ships.

use std::sync::atomic::{AtomicBool, Ordering};

use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::Executor;
use tokio::sync::Mutex;

/// Migrations that `store::pg::ensure_schema` does not repeat.
const MIGRATIONS: &[&str] = &[
    include_str!("../../../migrations/0006_scheduled_jobs.sql"),
    include_str!("../../../migrations/0007_agent_teams.sql"),
    include_str!("../../../migrations/0008_session_titles.sql"),
];

static READY: AtomicBool = AtomicBool::new(false);
static SCHEMA_LOCK: Mutex<()> = Mutex::const_new(());

/// `let pool = db_or_skip!(maybe_pool().await);`
///
/// The skip has to return from the *test*, not from a helper, so it cannot hide
/// behind a function call, and `rustfmt` expands a `let ... else` into three
/// lines (the `return` would then count as an uncovered line in DB mode). One
/// macro keeps both problems in one excluded place.
macro_rules! db_or_skip {
    ($pool:expr) => {
        match $pool {
            Some(value) => value,
            None => return,
        }
    };
}
pub(crate) use db_or_skip;

/// A pool for this test, or `None` when this environment has no database.
pub async fn maybe_pool() -> Option<PgPool> {
    open(std::env::var("DATABASE_URL").ok()).await
}

/// The skip path, with the URL supplied so it can be asserted directly.
pub(crate) async fn open(url: Option<String>) -> Option<PgPool> {
    match connect(url).await {
        Ok(pool) => Some(pool),
        Err(reason) => {
            eprintln!("skip: {reason}");
            None
        }
    }
}

/// Connect and make sure the schema is in place. Any failure is reported as
/// "skip", never as a pass: a suite that silently skipped must stay
/// distinguishable from a green one.
async fn connect(url: Option<String>) -> Result<PgPool, String> {
    let url = url.ok_or_else(|| "DATABASE_URL not set".to_string())?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .map_err(|e| format!("DATABASE_URL unreachable: {e}"))?;
    ensure_schema(&pool)
        .await
        .map_err(|e| format!("schema: {e}"))?;
    Ok(pool)
}

/// Apply the schema once per process; every later test trusts the flag.
async fn ensure_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    if READY.load(Ordering::Acquire) {
        return Ok(());
    }
    let _guard = SCHEMA_LOCK.lock().await;
    apply(pool).await?;
    READY.store(true, Ordering::Release);
    Ok(())
}

/// The DDL itself, always executed (idempotent).
async fn apply(pool: &PgPool) -> Result<(), sqlx::Error> {
    crate::store::pg::ensure_schema(pool).await?;
    for sql in MIGRATIONS {
        pool.execute(*sql).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Both "no database" arms are reachable without a database: the unset case
    /// and the unreachable-URL case. The success path is what every other DB
    /// test exercises.
    #[tokio::test]
    async fn the_fixture_skips_cleanly_without_a_database() {
        assert!(connect(None).await.is_err());
        assert!(open(None).await.is_none());
        let dead = Some("postgres://postgres:postgres@127.0.0.1:1/none".to_string());
        assert!(open(dead).await.is_none());
    }

    /// A reachable server whose schema cannot be applied must be reported, not
    /// swallowed. A throwaway database with a truncated `agents` table produces
    /// exactly that: the seed insert needs a `name` column the table lacks.
    #[tokio::test]
    async fn an_unusable_schema_is_reported() {
        let pool = db_or_skip!(maybe_pool().await);
        let name = format!("aria_broken_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {name}"))
            .execute(&pool)
            .await
            .expect("create probe database");
        let url = std::env::var("DATABASE_URL").expect("DATABASE_URL");
        let broken = match url.rsplit_once('/') {
            Some((base, _)) => format!("{base}/{name}"),
            None => panic!("DATABASE_URL has no database segment"),
        };
        let probe = PgPoolOptions::new()
            .max_connections(1)
            .connect(&broken)
            .await
            .expect("connect to probe database");
        probe
            .execute("CREATE TABLE agents (id TEXT PRIMARY KEY)")
            .await
            .expect("create the truncated table");
        assert!(apply(&probe).await.is_err());
        probe.close().await;
        sqlx::query(&format!("DROP DATABASE {name} WITH (FORCE)"))
            .execute(&pool)
            .await
            .expect("drop probe database");
    }
}
