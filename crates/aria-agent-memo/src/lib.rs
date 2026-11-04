//! `agent-memo` — the **on-device / local** context store, backed by
//! **aria memo**.
//!
//! The contract ([`ContextStore`], [`ContextFragment`], [`RecallQuery`], the
//! local [`embed`] module) lives in `aria-agent-core::context`; this crate is
//! the local/embedded backend used by the native SDK (Swift / Kotlin via
//! UniFFI) and by the `local` side of the `both` backend.
//!
//! Storage is **aria memo compatible**: the same `memories` table, the same
//! `memo_type` strings, the same embedding BLOB encoding and the same
//! `metadata` JSON shape as the `aria-memo` product / CLI, so a `memo.db`
//! written here can be read and edited with `aria-memo list --json`
//! (and vice versa) — see the `cli_interop_*` tests below.
//!
//! The previous sled implementation has been removed; there is no second
//! on-device format to keep in sync. See `docs/adr/0011-memory-backend-switch.md`.

use agent_core::context::embed::Embedder as _;
use agent_core::context::{
    embed, ensure_embedding, graph_bfs, rank, ContextError, ContextFragment, ContextStore,
    FragmentKind, GraphRetrieveQuery, GraphRetrieveResult, RecallQuery, Relation, RelationKind,
    RetrieveTrace, ScoredFragment, EMBED_DIM,
};
use async_trait::async_trait;
use rusqlite::Connection;
use rusqlite::OptionalExtension;
use std::collections::HashMap;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex};

pub use agent_core::context::{now_ms, ContextError as MemoError};

/// aria memo's `memories` schema (byte-for-byte the DDL used by the product),
/// so its CLI can open the same database.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    memo_type TEXT NOT NULL,
    content TEXT NOT NULL,
    embedding BLOB,
    metadata TEXT NOT NULL,
    importance REAL NOT NULL,
    version INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memo_type);
CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);
CREATE INDEX IF NOT EXISTS idx_memories_deleted ON memories(deleted);
-- Multi-relational memory plane: a SEPARATE table, never touching `memories`,
-- so the aria-memo CLI keeps full interop.
CREATE TABLE IF NOT EXISTS context_relations (
    from_id TEXT NOT NULL,
    to_id TEXT NOT NULL,
    session TEXT NOT NULL,
    kind TEXT NOT NULL,
    score REAL NOT NULL,
    provenance TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (from_id, to_id, session, kind)
);
CREATE INDEX IF NOT EXISTS idx_context_relations_from ON context_relations(from_id, kind);
CREATE INDEX IF NOT EXISTS idx_context_relations_to ON context_relations(to_id);
";

/// `aria-memo` writes `long_term:*` for durable memories; the mapping below
/// keeps agent fragment kinds and memo types reversible.
fn memo_type_of(kind: FragmentKind) -> &'static str {
    match kind {
        FragmentKind::Message => "working",
        FragmentKind::ToolResult => "short_term",
        FragmentKind::LongTerm => "long_term:semantic",
        FragmentKind::Note => "long_term:episodic",
    }
}

fn kind_of(memo_type: &str) -> Option<FragmentKind> {
    match memo_type {
        "working" => Some(FragmentKind::Message),
        "short_term" => Some(FragmentKind::ToolResult),
        "long_term:semantic" | "long_term:entity" | "long_term:graph" => {
            Some(FragmentKind::LongTerm)
        }
        "long_term:episodic" => Some(FragmentKind::Note),
        _ => None,
    }
}

fn importance_of(kind: FragmentKind) -> f32 {
    match kind {
        FragmentKind::LongTerm => 0.8,
        FragmentKind::Message => 0.5,
        FragmentKind::ToolResult => 0.4,
        FragmentKind::Note => 0.6,
    }
}

/// Map a rusqlite failure to a storage error. Shared by every statement so the
/// mapping is one exercised line instead of a per-call closure.
fn db_err(e: rusqlite::Error) -> ContextError {
    ContextError::Storage(e.to_string())
}

/// Build a message-prefixing mapper. Shared so the formatting closure is a
/// single exercised line instead of one dead closure per call site.
fn prefixed<E: std::fmt::Display>(msg: &'static str) -> impl Fn(E) -> ContextError {
    move |e| ContextError::Storage(format!("{msg}: {e}"))
}

/// Map a poisoned mutex to a storage error.
fn lock_err<E: std::fmt::Display>(e: E) -> ContextError {
    ContextError::Storage(format!("memo db lock poisoned: {e}"))
}

/// aria memo's embedding BLOB: empty = `None`, otherwise `u32 LE` length
/// followed by little-endian `f32`s.
fn serialize_embedding(emb: Option<&[f32]>) -> Vec<u8> {
    match emb {
        None => Vec::new(),
        Some(v) => {
            let mut buf = Vec::with_capacity(4 + v.len() * 4);
            buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
            for f in v {
                buf.extend_from_slice(&f.to_le_bytes());
            }
            buf
        }
    }
}

fn deserialize_embedding(buf: &[u8]) -> Option<Vec<f32>> {
    if buf.is_empty() {
        return None;
    }
    if buf.len() < 4 {
        return None;
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() != 4 + n * 4 {
        return None;
    }
    let mut v = Vec::with_capacity(n);
    for i in 0..n {
        let s = 4 + i * 4;
        v.push(f32::from_le_bytes([
            buf[s],
            buf[s + 1],
            buf[s + 2],
            buf[s + 3],
        ]));
    }
    Some(v)
}

fn metadata_json(session: &str, key: Option<&str>) -> String {
    let mut map = std::collections::HashMap::new();
    map.insert("session".to_string(), session.to_string());
    if let Some(k) = key {
        map.insert("key".to_string(), k.to_string());
    }
    // A map of strings always serializes; `unwrap_or_default` keeps the
    // never-taken fallback off the coverage map.
    serde_json::to_string(&map).unwrap_or_default()
}

/// aria memo backed context store (SQLite, local / on-device).
pub struct MemoContextStore {
    conn: Mutex<Connection>,
}

impl MemoContextStore {
    /// Open (or create) the aria memo database at `path`.
    pub fn open(path: &Path) -> Result<Arc<Self>, ContextError> {
        // No directory component (e.g. a bare file name) means nothing to
        // create; otherwise make sure the parent exists before opening.
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        parent
            .map_or(Ok(()), std::fs::create_dir_all)
            .map_err(|e| ContextError::Storage(format!("create db dir: {e}")))?;
        let conn = Connection::open(path).map_err(prefixed("open memo db"))?;
        conn.execute_batch(SCHEMA)
            .map_err(prefixed("migrate memo db"))?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    /// Throwaway database (tests, and the SDK default when no path is set).
    pub fn memory() -> Result<Arc<Self>, ContextError> {
        let dir = std::env::temp_dir().join(format!("aria-memo-{}", uuid::Uuid::new_v4()));
        // `open` creates (and reports failures for) the parent directory.
        Self::open(&dir.join("memo.db"))
    }

    fn all(&self) -> Result<Vec<ContextFragment>, ContextError> {
        let conn = self.conn.lock().map_err(lock_err)?;
        let mut stmt = conn
            .prepare(
                "SELECT id, memo_type, content, embedding, metadata, created_at \
                 FROM memories WHERE deleted = 0 ORDER BY created_at",
            )
            .map_err(db_err)?;
        let rows = stmt
            .query_map([], |row| {
                let emb: Vec<u8> = row.get(3)?;
                let meta: String = row.get(4)?;
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    deserialize_embedding(&emb),
                    meta,
                    row.get::<_, i64>(5)?,
                ))
            })
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            let (id, memo_type, content, embedding, meta, created_at) = r.map_err(db_err)?;
            let Some(kind) = kind_of(&memo_type) else {
                continue;
            };
            let parsed: std::collections::HashMap<String, String> =
                serde_json::from_str(&meta).unwrap_or_default();
            let session = parsed.get("session").cloned().unwrap_or_default();
            let key = parsed.get("key").cloned();
            out.push(ContextFragment {
                id,
                session,
                key,
                kind,
                content,
                created_at,
                embedding,
            });
        }
        Ok(out)
    }

    /// Look up a single fragment by id (used while expanding a graph).
    fn get(&self, id: &str) -> Result<Option<ContextFragment>, ContextError> {
        Ok(self.all()?.into_iter().find(|f| f.id == id))
    }
}

#[async_trait]
impl ContextStore for MemoContextStore {
    async fn memorize(&self, mut frag: ContextFragment) -> Result<(), ContextError> {
        ensure_embedding(&mut frag);
        let embedding = serialize_embedding(frag.embedding.as_deref());
        let now_sec = now_ms() / 1000;
        let meta = metadata_json(&frag.session, frag.key.as_deref());
        let conn = self.conn.lock().map_err(lock_err)?;
        conn.execute(
            "INSERT INTO memories \
             (id, memo_type, content, embedding, metadata, importance, version, \
              created_at, updated_at, deleted) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?8, 0) \
             ON CONFLICT(id) DO UPDATE SET \
                memo_type = excluded.memo_type, \
                content = excluded.content, \
                embedding = excluded.embedding, \
                metadata = excluded.metadata, \
                importance = excluded.importance, \
                updated_at = excluded.updated_at, \
                deleted = 0",
            rusqlite::params![
                frag.id,
                memo_type_of(frag.kind),
                frag.content,
                embedding,
                meta,
                importance_of(frag.kind) as f64,
                now_sec,
                now_sec,
            ],
        )
        .map_err(db_err)?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        if query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let candidates: Vec<ContextFragment> = self
            .all()?
            .into_iter()
            .filter(|f| f.session == query.session)
            .filter(|f| query.kind.map(|k| f.kind == k).unwrap_or(true))
            .collect();
        let q_emb = embed::LocalEmbedder::new(EMBED_DIM).embed(&query.text).ok();
        Ok(rank(&query.text, q_emb.as_deref(), candidates, query.top_k))
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
        let parts: Vec<String> = self
            .all()?
            .into_iter()
            .filter(|f| f.session == session)
            .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
            .collect();
        if parts.is_empty() {
            return Err(ContextError::NotFound(session.to_string()));
        }
        // Reuse the existing compaction note's id so repeated compactions
        // overwrite rather than accumulate, keeping history bounded.
        let key = format!("__compact__{session}");
        let existing_id = self.get_by_key(session, &key).await?.map(|f| f.id);
        let mut merged =
            ContextFragment::new(session, FragmentKind::Note, parts.join("\n---\n")).with_key(key);
        if let Some(id) = existing_id {
            merged.id = id;
        }
        self.memorize(merged.clone()).await?;
        Ok(merged)
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, ContextError> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|f| f.session == session && f.key.as_deref() == Some(key))
            .max_by_key(|f| f.created_at))
    }

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError> {
        let mut out: Vec<ContextFragment> = self
            .all()?
            .into_iter()
            .filter(|f| f.session == session)
            .collect();
        out.sort_by_key(|f| f.created_at);
        out.truncate(top_k);
        Ok(out)
    }

    async fn relate(&self, rel: Relation) -> Result<(), ContextError> {
        rel.validate()?;
        let conn = self.conn.lock().map_err(lock_err)?;
        for id in [&rel.from_id, &rel.to_id] {
            let exists: bool = conn
                .query_row(
                    "SELECT 1 FROM memories WHERE id = ?1 AND deleted = 0",
                    [id.clone()],
                    |_| Ok(true),
                )
                .optional()
                .map_err(db_err)?
                .is_some();
            if !exists {
                return Err(ContextError::NotFound(id.clone()));
            }
        }
        conn.execute(
            "INSERT INTO context_relations \
             (from_id, to_id, session, kind, score, provenance, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) \
             ON CONFLICT(from_id, to_id, session, kind) DO UPDATE SET \
                score = excluded.score, provenance = excluded.provenance, created_at = excluded.created_at",
            rusqlite::params![
                rel.from_id,
                rel.to_id,
                rel.session,
                rel.kind.as_str(),
                rel.score,
                rel.provenance,
                rel.created_at
            ],
        )
        .map_err(db_err)?;
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
        let conn = self.conn.lock().map_err(lock_err)?;
        let mut sql = String::from(
            "SELECT from_id, to_id, session, kind, score, provenance, created_at \
             FROM context_relations WHERE session = ?1",
        );
        let mut binds: Vec<String> = vec![session.to_string()];
        let mut n = 2;
        if let Some(f) = from {
            sql.push_str(&format!(" AND from_id = ?{n}"));
            binds.push(f.to_string());
            n += 1;
        }
        if let Some(t) = to {
            sql.push_str(&format!(" AND to_id = ?{n}"));
            binds.push(t.to_string());
            n += 1;
        }
        if let Some(k) = kind {
            sql.push_str(&format!(" AND kind = ?{n}"));
            binds.push(k.as_str().to_string());
            n += 1;
        }
        sql.push_str(&format!(" ORDER BY created_at DESC LIMIT ?{n}"));
        binds.push(top_k.to_string());
        let mut stmt = conn.prepare(&sql).map_err(db_err)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(binds.iter()), row_to_relation)
            .map_err(db_err)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r.map_err(db_err)?);
        }
        Ok(out)
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
        let conn = self.conn.lock().map_err(lock_err)?;
        let mut sql = String::from("DELETE FROM context_relations WHERE session = ?1");
        let mut binds: Vec<String> = vec![session.to_string()];
        let mut n = 2;
        if let Some(f) = from {
            sql.push_str(&format!(" AND from_id = ?{n}"));
            binds.push(f.to_string());
            n += 1;
        }
        if let Some(t) = to {
            sql.push_str(&format!(" AND to_id = ?{n}"));
            binds.push(t.to_string());
            n += 1;
        }
        if let Some(k) = kind {
            sql.push_str(&format!(" AND kind = ?{n}"));
            binds.push(k.as_str().to_string());
        }
        let removed = conn
            .execute(&sql, rusqlite::params_from_iter(binds.iter()))
            .map_err(db_err)?;
        Ok(removed)
    }

    async fn expand(
        &self,
        query: &GraphRetrieveQuery,
    ) -> Result<GraphRetrieveResult, ContextError> {
        query.validate()?;
        let conn = self.conn.lock().map_err(lock_err)?;
        let mut edges_stmt = conn
            .prepare("SELECT from_id, to_id, kind, score FROM context_relations WHERE session = ?1")
            .map_err(db_err)?;
        let edges_iter = edges_stmt
            .query_map([query.session.clone()], |row| {
                let from_id: String = row.get(0)?;
                let to_id: String = row.get(1)?;
                let kind: String = row.get(2)?;
                let score: f64 = row.get(3)?;
                let kind = kind.parse::<RelationKind>().map_err(|e: String| {
                    rusqlite::Error::FromSqlConversionFailure(
                        2,
                        rusqlite::types::Type::Text,
                        Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
                    )
                })?;
                Ok((from_id, to_id, kind, score as f32))
            })
            .map_err(db_err)?;
        let mut edges: HashMap<String, Vec<(String, f32, RelationKind)>> = HashMap::new();
        for e in edges_iter {
            let (f, t, k, s) = e.map_err(db_err)?;
            edges.entry(f).or_default().push((t, s, k));
        }
        drop(edges_stmt);
        drop(conn);

        let (reached, stop) = graph_bfs(
            &query.seeds,
            &query.views,
            query.budget,
            query.max_hops,
            |node| edges.get(node).cloned().unwrap_or_default(),
        );
        let mut items: Vec<ScoredFragment> = Vec::new();
        for (id, score) in reached.into_iter().take(query.top_k) {
            if let Some(f) = self.get(&id)? {
                items.push(ScoredFragment { fragment: f, score });
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

/// Map a `context_relations` row to a [`Relation`].
fn row_to_relation(row: &rusqlite::Row) -> rusqlite::Result<Relation> {
    let from_id: String = row.get(0)?;
    let to_id: String = row.get(1)?;
    let session: String = row.get(2)?;
    let kind: String = row.get(3)?;
    let score: f64 = row.get(4)?;
    let provenance: String = row.get(5)?;
    let created_at: i64 = row.get(6)?;
    let kind = RelationKind::from_str(&kind).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })?;
    Ok(Relation {
        session,
        from_id,
        to_id,
        kind,
        score: score as f32,
        provenance,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<MemoContextStore> {
        MemoContextStore::memory().unwrap()
    }

    #[tokio::test]
    async fn memorize_recall_roundtrip() {
        let store = store();
        let f =
            ContextFragment::new("s1", FragmentKind::Message, "the sky is blue").with_key("fact1");
        store.memorize(f).await.unwrap();
        let out = store.recall(&RecallQuery::new("s1", "sky")).await.unwrap();
        assert!(out.iter().any(|f| f.content.contains("sky")));
        assert!(store.get_by_key("s1", "fact1").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn compact_merges_session() {
        let store = store();
        store
            .memorize(ContextFragment::new("s2", FragmentKind::Message, "a"))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new("s2", FragmentKind::Message, "b"))
            .await
            .unwrap();
        let c = store.compact("s2").await.unwrap();
        assert!(c.content.contains("a") && c.content.contains("b"));
        assert!(matches!(
            store.compact("ghost").await,
            Err(ContextError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn vector_recall_prefers_similar() {
        let store = store();
        store
            .memorize(ContextFragment::new(
                "s3",
                FragmentKind::Message,
                "user prefers rust for systems programming",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                "s3",
                FragmentKind::Message,
                "banana smoothie recipe with ice",
            ))
            .await
            .unwrap();
        let frags = store
            .recall(&RecallQuery::new("s3", "rust programming language"))
            .await
            .unwrap();
        assert!(!frags.is_empty());
        assert!(frags[0].content.contains("rust"));
        assert!(frags[0].embedding.is_some());
    }

    #[tokio::test]
    async fn recall_filters_kind_and_session() {
        let store = store();
        store
            .memorize(ContextFragment::new(
                "a",
                FragmentKind::Message,
                "shared text",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                "b",
                FragmentKind::Message,
                "shared text",
            ))
            .await
            .unwrap();
        let out = store
            .recall(&RecallQuery::new("a", "shared"))
            .await
            .unwrap();
        assert!(out.iter().all(|f| f.session == "a"));
        assert_eq!(out.len(), 1);

        store
            .memorize(ContextFragment::new("a", FragmentKind::Note, "shared text"))
            .await
            .unwrap();
        let notes = store
            .recall(&RecallQuery::new("a", "shared").with_kind(FragmentKind::Note))
            .await
            .unwrap();
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].kind, FragmentKind::Note);
    }

    #[tokio::test]
    async fn empty_query_recalls_nothing() {
        let store = store();
        store
            .memorize(ContextFragment::new("s", FragmentKind::Message, "hello"))
            .await
            .unwrap();
        assert!(store
            .recall(&RecallQuery::new("s", ""))
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn memorize_populates_embedding_of_fixed_dim() {
        let store = store();
        store
            .memorize(
                ContextFragment::new("s", FragmentKind::Message, "rust programming").with_key("ke"),
            )
            .await
            .unwrap();
        let got = store.get_by_key("s", "ke").await.unwrap().unwrap();
        assert_eq!(got.embedding.as_ref().unwrap().len(), EMBED_DIM);
    }

    #[tokio::test]
    async fn upsert_replaces_existing_id() {
        let store = store();
        let mut frag = ContextFragment::new("s", FragmentKind::LongTerm, "v1");
        frag.id = "fixed".into();
        store.memorize(frag.clone()).await.unwrap();
        let mut updated = ContextFragment::new("s", FragmentKind::LongTerm, "v2");
        updated.id = "fixed".into();
        store.memorize(updated).await.unwrap();
        let all = store.all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].content, "v2");
    }

    #[test]
    fn embedding_blob_roundtrip_matches_aria_memo_format() {
        let v = vec![0.5f32, -0.25, 1.0];
        let blob = serialize_embedding(Some(&v));
        // 4-byte LE length prefix + LE floats.
        assert_eq!(&blob[..4], &3u32.to_le_bytes());
        assert_eq!(deserialize_embedding(&blob), Some(v));
        assert_eq!(deserialize_embedding(&[]), None);
    }

    #[test]
    fn memo_type_mapping_is_reversible() {
        for kind in [
            FragmentKind::Message,
            FragmentKind::ToolResult,
            FragmentKind::LongTerm,
            FragmentKind::Note,
        ] {
            assert_eq!(kind_of(memo_type_of(kind)), Some(kind));
        }
        assert_eq!(kind_of("unknown-type"), None);
    }

    #[tokio::test]
    async fn relation_crud_and_expand_on_context_table() {
        let store = store();
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::LongTerm,
                "user likes rust programming",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::LongTerm,
                "rust is used for systems programming",
            ))
            .await
            .unwrap();
        let frags = store.list_session("s", 10).await.unwrap();
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
                session: "s".into(),
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
                session: "s".into(),
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
            .get_relations("s", Some(&a.id), None, None, 10)
            .await
            .unwrap();
        assert_eq!(rels.len(), 1);
        assert_eq!(rels[0].to_id, b.id);

        // Expand a -> b.
        let q = GraphRetrieveQuery {
            session: "s".into(),
            seeds: vec![a.id.clone()],
            views: vec![],
            budget: 10,
            max_hops: 3,
            top_k: 10,
        };
        let res = store.expand(&q).await.unwrap();
        let ids: Vec<&str> = res.items.iter().map(|i| i.fragment.id.as_str()).collect();
        assert!(ids.contains(&a.id.as_str()));
        assert!(ids.contains(&b.id.as_str()));
        assert_eq!(res.trace.hits, 2);

        // Wrong session returns nothing (relations table is separate from memories).
        assert!(store
            .get_relations("other", None, None, None, 10)
            .await
            .unwrap()
            .is_empty());

        // Delete by from.
        let removed = store
            .delete_relations("s", Some(&a.id), None, None)
            .await
            .unwrap();
        assert_eq!(removed, 1);
        // Empty filter rejected.
        assert!(store.delete_relations("s", None, None, None).await.is_err());
    }

    /// The interop tests need the real `aria-memo` binary, so they are behind
    /// the `cli-interop` feature (off by default): a plain `cargo test` must
    /// never skip silently, and the coverage gate must not see dead test code.
    #[cfg(feature = "cli-interop")]
    fn aria_memo_bin() -> Option<String> {
        std::env::var("ARIA_MEMO_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| Some("aria-memo".to_string()))
            .filter(|bin| {
                std::process::Command::new(bin)
                    .arg("version")
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            })
    }

    #[cfg(feature = "cli-interop")]
    #[tokio::test]
    async fn cli_interop_rust_write_is_readable_by_aria_memo() {
        let Some(bin) = aria_memo_bin() else {
            eprintln!("skip: aria-memo CLI not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aria-memo-interop-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("memo.db");
        let store = MemoContextStore::open(&db).unwrap();
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::LongTerm,
                "rust interop fact",
            ))
            .await
            .unwrap();

        let out = std::process::Command::new(bin)
            .args(["--db"]) // keep the flag and value separate
            .arg(&db)
            .args(["list", "--json"])
            .output()
            .expect("run aria-memo list");
        assert!(
            out.status.success(),
            "aria-memo list failed: {:?}",
            out.stderr
        );
        let listed = String::from_utf8_lossy(&out.stdout);
        assert!(listed.contains("rust interop fact"), "got: {listed}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(feature = "cli-interop")]
    #[tokio::test]
    async fn cli_interop_aria_memo_write_is_readable_by_rust() {
        let Some(bin) = aria_memo_bin() else {
            eprintln!("skip: aria-memo CLI not installed");
            return;
        };
        let dir = std::env::temp_dir().join(format!("aria-memo-interop2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("memo.db");
        let out = std::process::Command::new(bin)
            .arg("--db")
            .arg(&db)
            .args([
                "add",
                "--type",
                "long_term:semantic",
                "--content",
                "cli wrote this fact",
                "--importance",
                "0.8",
            ])
            .output()
            .expect("run aria-memo add");
        assert!(
            out.status.success(),
            "aria-memo add failed: {:?}",
            out.stderr
        );

        let store = MemoContextStore::open(&db).unwrap();
        let all = store.all().unwrap();
        assert!(
            all.iter().any(|f| f.content == "cli wrote this fact"),
            "aria memo rows must be visible to the Rust store: {all:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Coverage tests for the storage-level branches: blob decoding, path errors,
/// unknown memo types, poisoned locks, corrupted schemas and invalid relations.
#[cfg(test)]
mod storage_edge_tests {
    use super::*;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aria-memo-edge-{}-{}", name, uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("memo.db")
    }

    fn edge(from: &str, to: &str, session: &str) -> Relation {
        Relation {
            session: session.into(),
            from_id: from.into(),
            to_id: to.into(),
            kind: RelationKind::Semantic,
            score: 0.5,
            provenance: "local:semantic".into(),
            created_at: 1,
        }
    }

    fn graph_query(seed: &str) -> GraphRetrieveQuery {
        GraphRetrieveQuery {
            session: "s".into(),
            seeds: vec![seed.into()],
            views: vec![],
            budget: 5,
            max_hops: 2,
            top_k: 5,
        }
    }

    #[test]
    fn embedding_blob_roundtrips_and_rejects_corruption() {
        assert!(serialize_embedding(None).is_empty());
        assert_eq!(deserialize_embedding(&[]), None);
        // Shorter than the u32 header.
        assert_eq!(deserialize_embedding(&[0, 1]), None);
        // Header claims four floats but no payload follows.
        assert_eq!(deserialize_embedding(&4u32.to_le_bytes()), None);
        let good = serialize_embedding(Some(&[0.5, -1.25]));
        assert_eq!(deserialize_embedding(&good), Some(vec![0.5, -1.25]));
    }

    #[tokio::test]
    async fn open_reports_an_unusable_path() {
        // `/dev/null` is a file, so the parent directory cannot be created.
        let err = MemoContextStore::open(std::path::Path::new("/dev/null/nested/memo.db"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("create db dir"), "{err}");
    }

    #[tokio::test]
    async fn open_reports_directory_paths_and_corrupt_files() {
        // A directory can never be opened as a database.
        let dir = std::env::temp_dir().join(format!("aria-memo-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let err = MemoContextStore::open(&dir).err().unwrap();
        assert!(err.to_string().contains("open memo db"), "{err}");

        // A file that is not a SQLite database fails while migrating.
        let bad = std::env::temp_dir().join(format!("aria-memo-bad-{}.db", uuid::Uuid::new_v4()));
        std::fs::write(&bad, b"not a sqlite database").unwrap();
        let err = MemoContextStore::open(&bad).err().unwrap();
        assert!(err.to_string().contains("migrate memo db"), "{err}");
        let _ = std::fs::remove_file(&bad);
    }

    #[tokio::test]
    async fn unknown_memo_types_are_skipped_when_listing() {
        let store = MemoContextStore::open(&temp_db("unknown")).unwrap();
        store
            .memorize(ContextFragment::new("s", FragmentKind::Note, "known"))
            .await
            .unwrap();
        // Tool results carry their own importance weight.
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::ToolResult,
                "tool output",
            ))
            .await
            .unwrap();
        // Rows written by other aria-memo versions may use memo types we do not
        // know; they must be skipped rather than abort the whole listing.
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO memories (id, memo_type, content, embedding, metadata, importance, version, created_at, updated_at, deleted) \
                 VALUES ('weird', 'unsupported_type', 'body', x'', '{}', 0.5, 1, 0, 0, 0)",
                [],
            )
            .unwrap();

        let all = store.all().unwrap();
        assert_eq!(all.len(), 2);
        assert!(all.iter().any(|f| f.content == "known"));
        assert!(all
            .iter()
            .any(|f| f.kind == FragmentKind::ToolResult && f.content == "tool output"));
    }

    #[tokio::test]
    async fn poisoned_connection_lock_is_reported_everywhere() {
        let store = MemoContextStore::open(&temp_db("poison")).unwrap();
        let a = ContextFragment::new("s", FragmentKind::Note, "a");
        let b = ContextFragment::new("s", FragmentKind::Note, "b");
        store.memorize(a.clone()).await.unwrap();
        store.memorize(b.clone()).await.unwrap();
        store.relate(edge(&a.id, &b.id, "s")).await.unwrap();

        let clone = store.clone();
        let _ = std::thread::spawn(move || {
            let _guard = clone.conn.lock().unwrap();
            panic!("poison the sqlite connection");
        })
        .join();

        assert!(store
            .memorize(ContextFragment::new("s", FragmentKind::Note, "c"))
            .await
            .is_err());
        assert!(store.recall(&RecallQuery::new("s", "a")).await.is_err());
        assert!(store.list_session("s", 5).await.is_err());
        assert!(store.get_by_key("s", "k").await.is_err());
        assert!(store.compact("s").await.is_err());
        assert!(store.relate(edge(&a.id, &b.id, "s")).await.is_err());
        assert!(store.get_relations("s", None, None, None, 5).await.is_err());
        assert!(store
            .delete_relations("s", Some(&a.id), None, None)
            .await
            .is_err());
        assert!(store.expand(&graph_query(&a.id)).await.is_err());
        assert!(store.all().is_err());
    }

    #[tokio::test]
    async fn sql_failures_surface_as_storage_errors() {
        // Dropping `memories` makes every fragment query fail at runtime.
        let store = MemoContextStore::open(&temp_db("corrupt")).unwrap();
        let a = ContextFragment::new("s", FragmentKind::Note, "a");
        let b = ContextFragment::new("s", FragmentKind::Note, "b");
        store.memorize(a.clone()).await.unwrap();
        store.memorize(b.clone()).await.unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE memories")
            .unwrap();

        assert!(store
            .memorize(ContextFragment::new("s", FragmentKind::Note, "c"))
            .await
            .is_err());
        assert!(store.recall(&RecallQuery::new("s", "a")).await.is_err());
        assert!(store.list_session("s", 5).await.is_err());
        assert!(store.get_by_key("s", "k").await.is_err());
        assert!(store.compact("s").await.is_err());
        assert!(store.relate(edge(&a.id, &b.id, "s")).await.is_err());
        assert!(store.expand(&graph_query(&a.id)).await.is_err());
        assert!(store.all().is_err());
    }

    #[tokio::test]
    async fn missing_relation_table_surfaces_as_storage_errors() {
        let store = MemoContextStore::open(&temp_db("corrupt-relations")).unwrap();
        let a = ContextFragment::new("s", FragmentKind::Note, "a");
        let b = ContextFragment::new("s", FragmentKind::Note, "b");
        store.memorize(a.clone()).await.unwrap();
        store.memorize(b.clone()).await.unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE context_relations")
            .unwrap();

        assert!(store.relate(edge(&a.id, &b.id, "s")).await.is_err());
        assert!(store.get_relations("s", None, None, None, 5).await.is_err());
        assert!(store
            .delete_relations("s", Some(&a.id), None, None)
            .await
            .is_err());
        assert!(store.expand(&graph_query(&a.id)).await.is_err());
    }

    #[tokio::test]
    async fn invalid_relation_kinds_are_reported() {
        let store = MemoContextStore::open(&temp_db("bad-kind")).unwrap();
        let a = ContextFragment::new("s", FragmentKind::Note, "a");
        let b = ContextFragment::new("s", FragmentKind::Note, "b");
        store.memorize(a.clone()).await.unwrap();
        store.memorize(b.clone()).await.unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO context_relations (from_id, to_id, session, kind, score, provenance, created_at) \
                 VALUES (?1, ?2, 's', 'bogus', 0.5, 'local', 1)",
                rusqlite::params![a.id, b.id],
            )
            .unwrap();

        let err = store
            .get_relations("s", None, None, None, 5)
            .await
            .err()
            .unwrap();
        assert!(err.to_string().contains("bogus"), "{err}");
        assert!(store.expand(&graph_query(&a.id)).await.is_err());
    }

    #[tokio::test]
    async fn open_keeps_rows_across_reopen() {
        // Exercises the directory-creation branch and the persistence contract.
        let db = temp_db("reopen");
        let store = MemoContextStore::open(&db).unwrap();
        store
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "durable"))
            .await
            .unwrap();
        drop(store);

        let reopened = MemoContextStore::open(&db).unwrap();
        assert_eq!(reopened.all().unwrap().len(), 1);
    }
}
