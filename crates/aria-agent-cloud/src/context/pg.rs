//! Postgres + pgvector implementation of the shared [`ContextStore`] contract.

use agent_core::context::embed::Embedder as _;
use agent_core::context::{
    embed, ensure_embedding, graph_bfs, rank, ContextError, ContextFragment, ContextStore,
    FragmentKind, GraphRetrieveQuery, GraphRetrieveResult, RecallQuery, Relation, RelationKind,
    RetrieveTrace, ScoredFragment, EMBED_DIM,
};
use async_trait::async_trait;
use sqlx::{Executor, PgPool, Row};
use std::collections::HashMap;
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
        // --- Multi-relational memory plane: relations between fragments ---
        "CREATE TABLE IF NOT EXISTS context_relations (\
            principal_id TEXT NOT NULL, \
            session_id TEXT NOT NULL, \
            from_id TEXT NOT NULL, \
            to_id TEXT NOT NULL, \
            kind TEXT NOT NULL, \
            score REAL NOT NULL, \
            provenance TEXT NOT NULL, \
            created_at BIGINT NOT NULL, \
            PRIMARY KEY (principal_id, session_id, from_id, to_id, kind))",
        "CREATE INDEX IF NOT EXISTS idx_context_relations_from \
         ON context_relations (principal_id, session_id, from_id, kind)",
        "CREATE INDEX IF NOT EXISTS idx_context_relations_to \
         ON context_relations (principal_id, session_id, to_id)",
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

fn row_to_relation(r: &sqlx::postgres::PgRow) -> Result<Relation, ContextError> {
    let kind: String = r.get("kind");
    let kind: RelationKind = kind
        .parse()
        .map_err(|e: String| ContextError::Storage(format!("bad relation kind: {e}")))?;
    Ok(Relation {
        session: r.get("session_id"),
        from_id: r.get("from_id"),
        to_id: r.get("to_id"),
        kind,
        score: r.get::<f64, _>("score") as f32,
        provenance: r.get("provenance"),
        created_at: r.get("created_at"),
    })
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

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError> {
        let rows = sqlx::query(&format!(
            "SELECT {SELECT_COLS} FROM context_fragments \
             WHERE principal_id = $1 AND session_id = $2 ORDER BY created_at LIMIT $3"
        ))
        .bind(&self.principal_id)
        .bind(session)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ContextError::Storage(e.to_string()))?;
        rows.iter().map(row_to_fragment).collect()
    }

    async fn relate(&self, rel: Relation) -> Result<(), ContextError> {
        rel.validate()?;
        // Both endpoints must exist for this tenant/session.
        for id in [&rel.from_id, &rel.to_id] {
            let exists: bool = sqlx::query(
                "SELECT 1 FROM context_fragments \
                 WHERE principal_id = $1 AND session_id = $2 AND id = $3",
            )
            .bind(&self.principal_id)
            .bind(&rel.session)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?
            .is_some();
            if !exists {
                return Err(ContextError::NotFound(id.clone()));
            }
        }
        sqlx::query(
            "INSERT INTO context_relations \
             (principal_id, session_id, from_id, to_id, kind, score, provenance, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT (principal_id, session_id, from_id, to_id, kind) DO UPDATE SET \
                score = EXCLUDED.score, provenance = EXCLUDED.provenance, created_at = EXCLUDED.created_at",
        )
        .bind(&self.principal_id)
        .bind(&rel.session)
        .bind(&rel.from_id)
        .bind(&rel.to_id)
        .bind(rel.kind.as_str())
        .bind(rel.score as f64)
        .bind(&rel.provenance)
        .bind(rel.created_at)
        .execute(&self.pool)
        .await
        .map_err(|e| ContextError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn get_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
        top_k: usize,
    ) -> Result<Vec<Relation>, ContextError> {
        let sql = "SELECT from_id, to_id, kind, score, provenance, created_at, session_id \
             FROM context_relations \
             WHERE principal_id = $1 AND session_id = $2 \
               AND ($3 IS NULL OR from_id = $3) \
               AND ($4 IS NULL OR to_id = $4) \
               AND ($5 IS NULL OR kind = $5) \
             ORDER BY created_at DESC LIMIT $6";
        let rows = sqlx::query(sql)
            .bind(&self.principal_id)
            .bind(session)
            .bind(from)
            .bind(to)
            .bind(kind.map(|k| k.as_str()))
            .bind(top_k as i64)
            .fetch_all(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        rows.iter().map(row_to_relation).collect()
    }

    async fn delete_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
    ) -> Result<usize, ContextError> {
        if from.is_none() && to.is_none() && kind.is_none() {
            return Err(ContextError::Storage(
                "delete_relations needs at least one filter".into(),
            ));
        }
        let sql = "DELETE FROM context_relations \
             WHERE principal_id = $1 AND session_id = $2 \
               AND ($3 IS NULL OR from_id = $3) \
               AND ($4 IS NULL OR to_id = $4) \
               AND ($5 IS NULL OR kind = $5)";
        let n = sqlx::query(sql)
            .bind(&self.principal_id)
            .bind(session)
            .bind(from)
            .bind(to)
            .bind(kind.map(|k| k.as_str()))
            .execute(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?
            .rows_affected();
        Ok(n as usize)
    }

    async fn expand(
        &self,
        query: &GraphRetrieveQuery,
    ) -> Result<GraphRetrieveResult, ContextError> {
        query.validate()?;
        // Load all edges for the session/tenant into memory, then traverse.
        let rows = sqlx::query(
            "SELECT from_id, to_id, kind, score FROM context_relations \
             WHERE principal_id = $1 AND session_id = $2",
        )
        .bind(&self.principal_id)
        .bind(&query.session)
        .fetch_all(&self.pool)
        .await
        .map_err(|e| ContextError::Storage(e.to_string()))?;
        let mut edges: HashMap<String, Vec<(String, f32, RelationKind)>> = HashMap::new();
        for r in &rows {
            let from_id: String = r.get("from_id");
            let to_id: String = r.get("to_id");
            let kind: String = r.get("kind");
            let kind: RelationKind = kind
                .parse()
                .map_err(|e: String| ContextError::Storage(format!("bad relation kind: {e}")))?;
            let score = r.get::<f64, _>("score") as f32;
            edges.entry(from_id).or_default().push((to_id, score, kind));
        }
        let (reached, stop) = graph_bfs(
            &query.seeds,
            &query.views,
            query.budget,
            query.max_hops,
            |node| edges.get(node).cloned().unwrap_or_default(),
        );
        let mut items: Vec<ScoredFragment> = Vec::new();
        for (id, score) in reached.into_iter().take(query.top_k) {
            let row = sqlx::query(&format!(
                "SELECT {SELECT_COLS} FROM context_fragments \
                 WHERE principal_id = $1 AND session_id = $2 AND id = $3"
            ))
            .bind(&self.principal_id)
            .bind(&query.session)
            .bind(&id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| ContextError::Storage(e.to_string()))?;
            if let Some(r) = row {
                items.push(ScoredFragment {
                    fragment: row_to_fragment(&r)?,
                    score,
                });
            }
        }
        let trace = RetrieveTrace {
            views: query.views.clone(),
            budget: query.budget,
            stop_reason: stop,
            hits: items.len(),
        };
        Ok(GraphRetrieveResult { items, trace })
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
    async fn pg_relations_roundtrip_when_database_url_is_set() {
        let Some(pool) = maybe_pool().await else {
            eprintln!("skip: DATABASE_URL not set");
            return;
        };
        let tenant = format!("test-{}", uuid::Uuid::new_v4());
        let session = format!("s-{}", uuid::Uuid::new_v4());
        let store = PgContextStore::new(pool, tenant);

        store
            .memorize(ContextFragment::new(
                &session,
                FragmentKind::LongTerm,
                "user likes rust programming",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                &session,
                FragmentKind::LongTerm,
                "rust is used for systems programming",
            ))
            .await
            .unwrap();
        let frags = store.list_session(&session, 10).await.unwrap();
        let a = frags
            .iter()
            .find(|f| f.content.contains("user likes"))
            .unwrap();
        let b = frags
            .iter()
            .find(|f| f.content.contains("rust is used"))
            .unwrap();

        // Missing endpoint rejected.
        assert!(store
            .relate(Relation {
                session: session.clone(),
                from_id: a.id.clone(),
                to_id: "ghost".into(),
                kind: RelationKind::Semantic,
                score: 0.8,
                provenance: "local".into(),
                created_at: 1,
            })
            .await
            .is_err());

        store
            .relate(Relation {
                session: session.clone(),
                from_id: a.id.clone(),
                to_id: b.id.clone(),
                kind: RelationKind::Semantic,
                score: 0.9,
                provenance: "local".into(),
                created_at: 1,
            })
            .await
            .unwrap();
        let rels = store
            .get_relations(&session, Some(&a.id), None, None, 10)
            .await
            .unwrap();
        assert_eq!(rels.len(), 1);

        // Expand a -> b.
        let q = GraphRetrieveQuery {
            session: session.clone(),
            seeds: vec![a.id.clone()],
            views: vec![],
            budget: 10,
            max_hops: 3,
            top_k: 10,
        };
        let res = store.expand(&q).await.unwrap();
        assert_eq!(res.trace.hits, 2);

        // Delete by from.
        let removed = store
            .delete_relations(&session, Some(&a.id), None, None)
            .await
            .unwrap();
        assert_eq!(removed, 1);
        assert!(store
            .delete_relations(&session, None, None, None)
            .await
            .is_err());
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

    #[tokio::test]
    async fn pg_relations_isolated_by_tenant() {
        let Some(pool) = maybe_pool().await else {
            eprintln!("skip: DATABASE_URL not set");
            return;
        };
        let session = format!("s-{}", uuid::Uuid::new_v4());
        let a = PgContextStore::new(
            pool.clone(),
            format!("rel-tenant-a-{}", uuid::Uuid::new_v4()),
        );
        let b = PgContextStore::new(pool, format!("rel-tenant-b-{}", uuid::Uuid::new_v4()));

        a.memorize(ContextFragment::new(
            &session,
            FragmentKind::LongTerm,
            "user likes rust programming",
        ))
        .await
        .unwrap();
        a.memorize(ContextFragment::new(
            &session,
            FragmentKind::LongTerm,
            "rust is used for systems programming",
        ))
        .await
        .unwrap();
        let frags = a.list_session(&session, 10).await.unwrap();
        let fa = frags
            .iter()
            .find(|f| f.content.contains("user likes"))
            .unwrap();
        let fb = frags
            .iter()
            .find(|f| f.content.contains("rust is used"))
            .unwrap();

        // Tenant A relates fa -> fb.
        a.relate(Relation {
            session: session.clone(),
            from_id: fa.id.clone(),
            to_id: fb.id.clone(),
            kind: RelationKind::Semantic,
            score: 0.9,
            provenance: "local".into(),
            created_at: 1,
        })
        .await
        .unwrap();

        // Tenant A sees its edge.
        assert_eq!(
            a.get_relations(&session, None, None, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        // Tenant B (same session id) sees nothing — principal_id scoping.
        assert!(b
            .get_relations(&session, None, None, None, 10)
            .await
            .unwrap()
            .is_empty());

        // Tenant B's graph expansion from fa's id reaches no fragments.
        let q = GraphRetrieveQuery {
            session: session.clone(),
            seeds: vec![fa.id.clone()],
            views: vec![],
            budget: 10,
            max_hops: 3,
            top_k: 10,
        };
        let res = b.expand(&q).await.unwrap();
        assert_eq!(res.trace.hits, 0);
    }
}
