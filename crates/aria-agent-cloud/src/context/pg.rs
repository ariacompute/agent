//! Postgres + pgvector implementation of the shared [`ContextStore`] contract.

use agent_core::context::embed::Embedder as _;
use agent_core::context::{
    embed, ensure_embedding, rank, ContextError, ContextFragment, ContextStore, FragmentKind,
    RecallQuery, EMBED_DIM,
};
use async_trait::async_trait;
use sqlx::{Executor, PgPool, Row};
use std::sync::Arc;

/// Columns read by every query. `embedding` is fetched as text so sqlx does not
/// need a pgvector type mapping.
const SELECT_COLS: &str = "id, session_id, fragment_key, kind, content, created_at, \
     embedding::text AS embedding";

/// Create the context table (idempotent; safe to replay on every boot).
pub async fn ensure_schema(pool: &PgPool) -> Result<(), sqlx::Error> {
    let ddl = format!(
        "CREATE TABLE IF NOT EXISTS context_fragments (\
            id TEXT PRIMARY KEY, \
            principal_id TEXT NOT NULL, \
            session_id TEXT NOT NULL, \
            fragment_key TEXT, \
            kind TEXT NOT NULL, \
            content TEXT NOT NULL, \
            created_at BIGINT NOT NULL, \
            embedding vector({EMBED_DIM}))"
    );
    for stmt in [
        "CREATE EXTENSION IF NOT EXISTS vector",
        ddl.as_str(),
        // btree pre-filter (tenant + session) before the vector scan.
        "CREATE INDEX IF NOT EXISTS idx_context_fragments_tenant_session \
         ON context_fragments (principal_id, session_id)",
        "CREATE INDEX IF NOT EXISTS idx_context_fragments_kind \
         ON context_fragments (principal_id, session_id, kind)",
        // Approximate nearest neighbour index for cosine distance.
        "CREATE INDEX IF NOT EXISTS idx_context_fragments_embedding \
         ON context_fragments USING hnsw (embedding vector_cosine_ops)",
    ] {
        pool.execute(stmt).await?;
    }
    Ok(())
}

/// A tenant-scoped context store backed by Postgres + pgvector.
///
/// One instance per [`Principal`](crate::Principal): `principal_id` is part of
/// every WHERE clause, so tenants can never read each other's context.
pub struct PgContextStore {
    pool: PgPool,
    principal_id: String,
}

impl PgContextStore {
    pub fn new(pool: PgPool, principal_id: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            pool,
            principal_id: principal_id.into(),
        })
    }

    /// Tenant id this store is scoped to (every query filters on it).
    pub fn principal_id(&self) -> &str {
        &self.principal_id
    }

    /// Number of candidates pulled from Postgres before Rust-side re-ranking.
    fn candidate_limit(top_k: usize) -> i64 {
        (top_k.saturating_mul(4).max(20)) as i64
    }

    /// Every fragment of a session, oldest first.
    async fn fetch_session(&self, session: &str) -> Result<Vec<ContextFragment>, ContextError> {
        let sql = format!(
            "SELECT {SELECT_COLS} FROM context_fragments \
             WHERE principal_id = $1 AND session_id = $2 ORDER BY created_at"
        );
        let rows = sqlx::query(&sql)
            .bind(&self.principal_id)
            .bind(session)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        rows.iter().map(row_to_fragment).collect()
    }
}

fn row_to_fragment(r: &sqlx::postgres::PgRow) -> Result<ContextFragment, ContextError> {
    let kind: String = r.get("kind");
    let kind: FragmentKind = kind
        .parse()
        .map_err(|e: String| ContextError::Storage(format!("bad fragment kind: {e}")))?;
    let embedding: Option<String> = r.get("embedding");
    Ok(ContextFragment {
        id: r.get("id"),
        session: r.get("session_id"),
        key: r.get("fragment_key"),
        kind,
        content: r.get("content"),
        created_at: r.get("created_at"),
        embedding: embedding.as_deref().and_then(parse_vector),
    })
}

/// `[0.1,0.2,...]` — pgvector's text representation.
fn to_vector(v: &[f32]) -> String {
    let mut s = String::from("[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!("{x}"));
    }
    s.push(']');
    s
}

fn parse_vector(s: &str) -> Option<Vec<f32>> {
    let inner = s.trim().trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return Some(Vec::new());
    }
    inner
        .split(',')
        .map(|p| p.trim().parse::<f32>().ok())
        .collect()
}

#[async_trait]
impl ContextStore for PgContextStore {
    async fn memorize(&self, mut frag: ContextFragment) -> Result<(), ContextError> {
        ensure_embedding(&mut frag);
        let embedding = frag.embedding.as_deref().map(to_vector);
        sqlx::query(
            "INSERT INTO context_fragments \
             (id, principal_id, session_id, fragment_key, kind, content, created_at, embedding) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8::vector) \
             ON CONFLICT (id) DO UPDATE SET \
                fragment_key = EXCLUDED.fragment_key, \
                kind = EXCLUDED.kind, \
                content = EXCLUDED.content, \
                created_at = EXCLUDED.created_at, \
                embedding = EXCLUDED.embedding",
        )
        .bind(&frag.id)
        .bind(&self.principal_id)
        .bind(&frag.session)
        .bind(&frag.key)
        .bind(frag.kind.as_str())
        .bind(&frag.content)
        .bind(frag.created_at)
        .bind(embedding)
        .execute(&self.pool)
        .await
        .map_err(|e| ContextError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        if query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let q_emb = embed::LocalEmbedder::new(EMBED_DIM).embed(&query.text).ok();
        let kind_clause = match query.kind {
            Some(k) => format!(" AND kind = '{k}'", k = k.as_str()),
            None => String::new(),
        };
        let limit = Self::candidate_limit(query.top_k);

        let mut merged: std::collections::HashMap<String, ContextFragment> =
            std::collections::HashMap::new();

        // 1) Vector candidates (pgvector cosine distance).
        if let Some(q) = q_emb.as_deref() {
            let sql = format!(
                "SELECT {SELECT_COLS} FROM context_fragments \
                 WHERE principal_id = $1 AND session_id = $2 AND embedding IS NOT NULL{kind_clause} \
                 ORDER BY embedding <=> $3::vector LIMIT {limit}"
            );
            let rows = sqlx::query(&sql)
                .bind(&self.principal_id)
                .bind(&query.session)
                .bind(to_vector(q))
                .fetch_all(&self.pool)
                .await
                .map_err(|e| ContextError::Storage(e.to_string()))?;
            for r in &rows {
                let f = row_to_fragment(r)?;
                merged.insert(f.id.clone(), f);
            }
        }

        // 2) Keyword / exact-key candidates (`strpos` avoids LIKE wildcard
        //    injection from user text).
        let sql = format!(
            "SELECT {SELECT_COLS} FROM context_fragments \
             WHERE principal_id = $1 AND session_id = $2 \
               AND (fragment_key = $3 OR strpos(lower(content), lower($4)) > 0){kind_clause} \
             LIMIT {limit}"
        );
        let rows = sqlx::query(&sql)
            .bind(&self.principal_id)
            .bind(&query.session)
            .bind(&query.text)
            .bind(&query.text)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        for r in &rows {
            let f = row_to_fragment(r)?;
            merged.insert(f.id.clone(), f);
        }

        let candidates: Vec<ContextFragment> = merged.into_values().collect();
        Ok(rank(&query.text, q_emb.as_deref(), candidates, query.top_k))
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
        let frags = self.fetch_session(session).await?;
        if frags.is_empty() {
            return Err(ContextError::NotFound(session.to_string()));
        }
        let parts: Vec<String> = frags
            .iter()
            .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
            .collect();
        let merged = ContextFragment::new(session, FragmentKind::Note, parts.join("\n---\n"))
            .with_key(format!("__compact__{session}"));
        self.memorize(merged.clone()).await?;
        Ok(merged)
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, ContextError> {
        let row = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM context_fragments \
             WHERE principal_id = $1 AND session_id = $2 AND fragment_key = $3 \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(&self.principal_id)
        .bind(session)
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| ContextError::Storage(e.to_string()))?;
        row.as_ref().map(row_to_fragment).transpose()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_text_roundtrip() {
        let v = vec![0.5f32, -0.25, 1.0];
        let s = to_vector(&v);
        assert_eq!(s, "[0.5,-0.25,1]");
        assert_eq!(parse_vector(&s), Some(v));
    }

    #[test]
    fn parse_vector_rejects_garbage() {
        assert_eq!(parse_vector("[]"), Some(Vec::new()));
        assert!(parse_vector("[a,b]").is_none());
    }

    #[test]
    fn candidate_limit_is_bounded() {
        assert!(PgContextStore::candidate_limit(8) >= 20);
        assert_eq!(PgContextStore::candidate_limit(0), 20);
    }

    /// DB-gated: skipped unless `DATABASE_URL` is set.
    ///
    /// Requires the `vector` extension (`pgvector/pgvector:pg16`).
    async fn maybe_pool() -> Option<PgPool> {
        let url = std::env::var("DATABASE_URL").ok()?;
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(2)
            .connect(&url)
            .await
            .expect("DATABASE_URL is set but unreachable");
        ensure_schema(&pool).await.expect("context schema");
        Some(pool)
    }

    #[tokio::test]
    async fn pg_store_roundtrip_when_database_url_is_set() {
        let Some(pool) = maybe_pool().await else {
            eprintln!("skip: DATABASE_URL not set");
            return;
        };
        // Use a unique tenant/session so repeated runs never collide.
        let tenant = format!("test-{}", uuid::Uuid::new_v4());
        let session = format!("s-{}", uuid::Uuid::new_v4());
        let store = PgContextStore::new(pool, tenant);

        store
            .memorize(
                ContextFragment::new(&session, FragmentKind::Message, "the sky is blue")
                    .with_key("fact1"),
            )
            .await
            .unwrap();

        let out = store
            .recall(&RecallQuery::new(&session, "sky"))
            .await
            .unwrap();
        assert!(out.iter().any(|f| f.content.contains("sky")));

        let by_key = store.get_by_key(&session, "fact1").await.unwrap();
        assert!(by_key.is_some());
        assert!(by_key.unwrap().embedding.is_some());

        // Empty query recalls nothing (abnormal path).
        assert!(store
            .recall(&RecallQuery::new(&session, ""))
            .await
            .unwrap()
            .is_empty());

        let compacted = store.compact(&session).await.unwrap();
        assert!(compacted.content.contains("sky is blue"));
    }

    #[tokio::test]
    async fn pg_store_isolates_tenants_and_sessions() {
        let Some(pool) = maybe_pool().await else {
            eprintln!("skip: DATABASE_URL not set");
            return;
        };
        let session = format!("s-{}", uuid::Uuid::new_v4());
        let a = PgContextStore::new(pool.clone(), format!("tenant-a-{}", uuid::Uuid::new_v4()));
        let b = PgContextStore::new(pool, format!("tenant-b-{}", uuid::Uuid::new_v4()));
        a.memorize(ContextFragment::new(
            &session,
            FragmentKind::Message,
            "shared text",
        ))
        .await
        .unwrap();

        let out_a = a
            .recall(&RecallQuery::new(&session, "shared"))
            .await
            .unwrap();
        assert_eq!(out_a.len(), 1);
        // Tenant B must not see tenant A's context.
        let out_b = b
            .recall(&RecallQuery::new(&session, "shared"))
            .await
            .unwrap();
        assert!(out_b.is_empty());
        assert!(matches!(
            b.compact(&session).await,
            Err(ContextError::NotFound(_))
        ));
    }
}
