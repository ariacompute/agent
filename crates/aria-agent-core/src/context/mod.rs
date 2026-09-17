//! Context memory contract shared by every runtime tier.
//!
//! Historically this lived in `aria-agent-memo` as `MemoStore` (local/embedded).
//! The contract is lifted into `agent-core` so the cloud can ship a
//! **Postgres + pgvector** implementation (see
//! `crates/aria-agent-cloud/src/context/pg.rs`) while the on-device SDK keeps
//! the **aria memo** implementation (`aria-agent-memo`). The two share:
//!
//! * [`ContextStore`] — the storage contract (`memorize` / `recall` / `compact`
//!   / `get_by_key`),
//! * [`ContextFragment`] / [`FragmentKind`] / [`RecallQuery`] — the data shapes,
//! * [`embed`] — the zero-dependency local embedder used for semantic recall,
//! * [`score_fragment`] / [`rank`] — the keyword + vector blending rules.
//!
//! See `docs/adr/0010-context-storage-pgvector.md`.

use async_trait::async_trait;
use embed::Embedder as _;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use thiserror::Error;
use uuid::Uuid;

/// Dimension of the dense embedding produced by [`embed::LocalEmbedder`].
///
/// Fixed so every backend (Postgres `vector(256)`, aria memo, in-memory)
/// agrees on the vector width; [`embed::cosine`] returns `None` on a width
/// mismatch.
pub const EMBED_DIM: usize = 256;

/// Kinds of context fragments a store can hold.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FragmentKind {
    /// A turn of the conversation (user message or assistant reply).
    Message,
    /// A tool call result that should be remembered across turns.
    ToolResult,
    /// An explicitly externalized long-term memory written via `memorize`.
    LongTerm,
    /// A system / scratch note.
    Note,
}

impl FragmentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            FragmentKind::Message => "message",
            FragmentKind::ToolResult => "tool_result",
            FragmentKind::LongTerm => "long_term",
            FragmentKind::Note => "note",
        }
    }
}

impl FromStr for FragmentKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "message" => Ok(FragmentKind::Message),
            "tool_result" => Ok(FragmentKind::ToolResult),
            "long_term" => Ok(FragmentKind::LongTerm),
            "note" => Ok(FragmentKind::Note),
            other => Err(format!("unknown fragment kind: {other}")),
        }
    }
}

/// A single unit of remembered context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextFragment {
    pub id: String,
    pub session: String,
    /// Optional explicit key (used by `Session::memorize`/`recall`).
    pub key: Option<String>,
    pub kind: FragmentKind,
    pub content: String,
    /// Unix epoch milliseconds.
    pub created_at: i64,
    /// Optional dense vector for semantic (vector) recall. Populated by the
    /// [`embed::LocalEmbedder`] when a fragment is memorized without one.
    pub embedding: Option<Vec<f32>>,
}

impl ContextFragment {
    pub fn new(session: &str, kind: FragmentKind, content: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            session: session.to_string(),
            key: None,
            kind,
            content: content.into(),
            created_at: now_ms(),
            embedding: None,
        }
    }

    pub fn with_key(mut self, key: impl Into<String>) -> Self {
        self.key = Some(key.into());
        self
    }
}

/// Query used by [`ContextStore::recall`].
#[derive(Debug, Clone)]
pub struct RecallQuery {
    pub session: String,
    pub text: String,
    pub top_k: usize,
    pub kind: Option<FragmentKind>,
}

impl RecallQuery {
    pub fn new(session: &str, text: impl Into<String>) -> Self {
        Self {
            session: session.to_string(),
            text: text.into(),
            top_k: 8,
            kind: None,
        }
    }

    pub fn with_kind(mut self, kind: FragmentKind) -> Self {
        self.kind = Some(kind);
        self
    }
}

#[derive(Debug, Error)]
pub enum ContextError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("embedding error: {0}")]
    Embedding(String),
}

/// Where a memory context lives.
///
/// * `Cloud` — the agent-cloud store (Postgres + pgvector), shared across
///   devices and instances.
/// * `Local` — the on-device **aria memo** store (SQLite), works offline.
/// * `Both` — write to both, read merged (see [`CompositeContextStore`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MemoryBackend {
    #[default]
    Cloud,
    Local,
    Both,
}

impl MemoryBackend {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryBackend::Cloud => "cloud",
            MemoryBackend::Local => "local",
            MemoryBackend::Both => "both",
        }
    }
}

impl std::str::FromStr for MemoryBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "cloud" => Ok(MemoryBackend::Cloud),
            "local" => Ok(MemoryBackend::Local),
            "both" => Ok(MemoryBackend::Both),
            other => Err(format!("unknown memory backend: {other}")),
        }
    }
}

impl std::fmt::Display for MemoryBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The unified context-memory contract. Everything that needs conversational
/// or long-term context goes through this trait.
///
/// Implementations:
/// * `aria-agent-memo::MemoContextStore` — on-device / local (aria memo, SQLite),
/// * `aria-agent-cloud::context::PgContextStore` — cloud (Postgres + pgvector),
/// * [`CompositeContextStore`] — `both`: writes to two stores, reads merged.
#[async_trait]
pub trait ContextStore: Send + Sync {
    /// Persist a fragment.
    async fn memorize(&self, frag: ContextFragment) -> Result<(), ContextError>;
    /// Retrieve the most relevant fragments for a query.
    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError>;
    /// Merge a session's fragments into a single compact fragment.
    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError>;
    /// Lookup an explicit long-term memory by key (used by SDK `recall`).
    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, ContextError>;
    /// Every fragment of a session, oldest first (used by `compact` and by the
    /// session memory listing).
    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError>;
}

/// Embed a fragment's content when it carries no vector yet. Shared by every
/// backend so stored vectors stay comparable.
pub fn ensure_embedding(frag: &mut ContextFragment) {
    if frag.embedding.is_none() && !frag.content.trim().is_empty() {
        if let Ok(v) = embed::LocalEmbedder::new(EMBED_DIM).embed(&frag.content) {
            frag.embedding = Some(v);
        }
    }
}

/// Keyword relevance of `frag` against `query_text`:
/// * `1.0` when the fragment's explicit key matches the query exactly,
/// * otherwise a length-normalized count of query words found in the content.
pub fn keyword_score(query_text: &str, frag: &ContextFragment) -> f32 {
    let q = query_text.to_lowercase();
    let mut kw = 0.0f32;
    if frag.key.as_deref() == Some(query_text) {
        kw = 1.0;
    }
    let hay = frag.content.to_lowercase();
    if !q.is_empty() && hay.contains(&q) {
        let hits = q
            .split_whitespace()
            .filter(|w| !w.is_empty() && hay.contains(*w))
            .count() as f32;
        kw = kw.max(hits / (hay.len() as f32).max(1.0).log10());
    }
    kw
}

/// Blend semantic (vector) similarity with the keyword score: `0.7 * cosine +
/// 0.3 * keyword`. Pure keyword when a vector is missing on either side.
pub fn score_fragment(
    query_text: &str,
    query_embedding: Option<&[f32]>,
    frag: &ContextFragment,
) -> f32 {
    let kw = keyword_score(query_text, frag);
    match (query_embedding, frag.embedding.as_deref()) {
        (Some(a), Some(b)) => match embed::cosine(a, b) {
            Some(v) => 0.7 * v + 0.3 * kw,
            None => kw,
        },
        _ => kw,
    }
}

/// Score, sort (descending) and truncate candidates to `top_k`. Fragments that
/// score `0.0` are dropped — an empty query therefore recalls nothing.
pub fn rank(
    query_text: &str,
    query_embedding: Option<&[f32]>,
    mut frags: Vec<ContextFragment>,
    top_k: usize,
) -> Vec<ContextFragment> {
    let mut scored: Vec<(f32, ContextFragment)> = frags
        .drain(..)
        .map(|f| (score_fragment(query_text, query_embedding, &f), f))
        .filter(|(s, _)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(top_k);
    scored.into_iter().map(|(_, f)| f).collect()
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Merge results from two backends: a fragment is a duplicate when **either**
/// its `id` or its `key` was already seen; the freshest copy wins.
pub fn merge_fragments(a: Vec<ContextFragment>, b: Vec<ContextFragment>) -> Vec<ContextFragment> {
    let mut out: Vec<ContextFragment> = Vec::with_capacity(a.len() + b.len());
    let mut index: HashMap<String, usize> = HashMap::new();
    for frag in a.into_iter().chain(b) {
        let ids: Vec<String> = [
            Some(frag.id.clone()).filter(|s| !s.is_empty()),
            frag.key.clone(),
        ]
        .into_iter()
        .flatten()
        .collect();
        let existing = ids.iter().find_map(|k| index.get(k).copied());
        match existing {
            Some(i) if frag.created_at > out[i].created_at => {
                for k in &ids {
                    index.insert(k.clone(), i);
                }
                out[i] = frag;
            }
            Some(_) => {}
            None => {
                let i = out.len();
                for k in &ids {
                    index.insert(k.clone(), i);
                }
                out.push(frag);
            }
        }
    }
    out
}

/// The `both` backend: a local (aria memo) store plus a cloud store.
///
/// * `memorize` writes to both (local first, so an offline write still lands);
///   a single-side failure is logged and tolerated — only a total failure
///   returns an error.
/// * `recall` queries both, merges, dedupes and re-ranks with the shared
///   [`rank`] scoring so ordering matches a single-backend run.
/// * `get_by_key` prefers the cloud copy and falls back to local.
pub struct CompositeContextStore {
    local: Arc<dyn ContextStore>,
    cloud: Arc<dyn ContextStore>,
}

impl CompositeContextStore {
    pub fn new(local: Arc<dyn ContextStore>, cloud: Arc<dyn ContextStore>) -> Arc<Self> {
        Arc::new(Self { local, cloud })
    }

    pub fn local(&self) -> &Arc<dyn ContextStore> {
        &self.local
    }

    pub fn cloud(&self) -> &Arc<dyn ContextStore> {
        &self.cloud
    }
}

#[async_trait]
impl ContextStore for CompositeContextStore {
    async fn memorize(&self, frag: ContextFragment) -> Result<(), ContextError> {
        let local = self.local.memorize(frag.clone()).await;
        let cloud = self.cloud.memorize(frag).await;
        match (local, cloud) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(e)) => {
                tracing::warn!("context: cloud memorize failed, kept local copy: {e}");
                Ok(())
            }
            (Err(e), Ok(())) => {
                tracing::warn!("context: local memorize failed, kept cloud copy: {e}");
                Ok(())
            }
            (Err(e), Err(_)) => Err(e),
        }
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        let local = self.local.recall(query).await;
        let cloud = self.cloud.recall(query).await;
        let (local, cloud) = match (local, cloud) {
            (Ok(l), Ok(c)) => (l, c),
            (Ok(l), Err(e)) => {
                tracing::warn!("context: cloud recall failed, using local only: {e}");
                (l, Vec::new())
            }
            (Err(e), Ok(c)) => {
                tracing::warn!("context: local recall failed, using cloud only: {e}");
                (Vec::new(), c)
            }
            (Err(e), Err(_)) => return Err(e),
        };
        let merged = merge_fragments(local, cloud);
        let q_emb = embed::LocalEmbedder::new(EMBED_DIM).embed(&query.text).ok();
        Ok(rank(&query.text, q_emb.as_deref(), merged, query.top_k))
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
        // Compact from whichever side has the session; the cloud copy wins when
        // both do, and the result is persisted back to both.
        let compacted = match self.cloud.compact(session).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("context: cloud compact failed, using local: {e}");
                match self.local.compact(session).await {
                    Ok(c) => c,
                    Err(e) => return Err(e),
                }
            }
        };
        self.memorize(compacted.clone()).await?;
        Ok(compacted)
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, ContextError> {
        match self.cloud.get_by_key(session, key).await {
            Ok(Some(f)) => Ok(Some(f)),
            Ok(None) => self.local.get_by_key(session, key).await,
            Err(e) => {
                tracing::warn!("context: cloud get_by_key failed, trying local: {e}");
                self.local.get_by_key(session, key).await
            }
        }
    }

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError> {
        let local = self.local.list_session(session, top_k).await;
        let cloud = self.cloud.list_session(session, top_k).await;
        let (local, cloud) = match (local, cloud) {
            (Ok(l), Ok(c)) => (l, c),
            (Ok(l), Err(e)) => {
                tracing::warn!("context: cloud list failed, using local only: {e}");
                (l, Vec::new())
            }
            (Err(e), Ok(c)) => {
                tracing::warn!("context: local list failed, using cloud only: {e}");
                (Vec::new(), c)
            }
            (Err(e), Err(_)) => return Err(e),
        };
        let mut merged = merge_fragments(local, cloud);
        merged.sort_by_key(|f| f.created_at);
        merged.truncate(top_k);
        Ok(merged)
    }
}

/// In-process [`ContextStore`] used by tests and by the SDK default.
///
/// Kept in `agent-core` so core's tests need no on-device database (and no
/// dependency on the embedded store crate).
pub struct MemoryContextStore {
    inner: RwLock<HashMap<String, ContextFragment>>,
}

impl MemoryContextStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
        })
    }
}

impl Default for MemoryContextStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl ContextStore for MemoryContextStore {
    async fn memorize(&self, mut frag: ContextFragment) -> Result<(), ContextError> {
        ensure_embedding(&mut frag);
        let mut g = self
            .inner
            .write()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        g.insert(frag.id.clone(), frag);
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        let g = self
            .inner
            .read()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        if query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let q_emb = if query.text.trim().is_empty() {
            None
        } else {
            embed::LocalEmbedder::new(EMBED_DIM).embed(&query.text).ok()
        };
        let candidates: Vec<ContextFragment> = g
            .values()
            .filter(|f| f.session == query.session)
            .filter(|f| query.kind.map(|k| f.kind == k).unwrap_or(true))
            .cloned()
            .collect();
        Ok(rank(&query.text, q_emb.as_deref(), candidates, query.top_k))
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
        let parts: Vec<String> = {
            let g = self
                .inner
                .read()
                .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
            g.values()
                .filter(|f| f.session == session)
                .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
                .collect()
        };
        if parts.is_empty() {
            return Err(ContextError::NotFound(session.to_string()));
        }
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
        let g = self
            .inner
            .read()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        Ok(g.values()
            .find(|f| f.session == session && f.key.as_deref() == Some(key))
            .cloned())
    }

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError> {
        let g = self
            .inner
            .read()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        let mut out: Vec<ContextFragment> = g
            .values()
            .filter(|f| f.session == session)
            .cloned()
            .collect();
        out.sort_by_key(|f| f.created_at);
        out.truncate(top_k);
        Ok(out)
    }
}

/// Local lightweight embedding + cosine similarity for vector recall.
///
/// Uses the hashing trick (word 1~2-grams + char 2-grams via FNV-1a) into a
/// fixed-dim vector, TF-normalized then L2-normalized. Zero external/ML deps.
pub mod embed {
    use super::ContextError;
    use std::collections::HashMap;

    /// Text embedding seam.
    pub trait Embedder: Send + Sync {
        fn embed(&self, text: &str) -> Result<Vec<f32>, ContextError>;
        fn dim(&self) -> usize;
    }

    /// Hashing-trick local embedder.
    pub struct LocalEmbedder {
        dim: usize,
    }

    impl LocalEmbedder {
        pub fn new(dim: usize) -> Self {
            Self { dim: dim.max(1) }
        }

        fn vectorize(&self, text: &str) -> Result<Vec<f32>, ContextError> {
            let toks = tokenize(text);
            if toks.is_empty() {
                return Err(ContextError::Embedding("empty embedding text".into()));
            }
            let mut vec = vec![0.0f32; self.dim];
            let mut counts: HashMap<usize, f32> = HashMap::new();
            for t in &toks {
                let h = hash_dim(t, self.dim);
                *counts.entry(h).or_insert(0.0) += 1.0;
            }
            let max = counts.values().cloned().fold(1.0f32, f32::max);
            for (h, c) in counts {
                vec[h] = (c / max).sqrt();
            }
            let norm = vec.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm == 0.0 {
                return Err(ContextError::Embedding("zero-magnitude vector".into()));
            }
            for v in vec.iter_mut() {
                *v /= norm;
            }
            Ok(vec)
        }
    }

    impl Embedder for LocalEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, ContextError> {
            self.vectorize(text)
        }
        fn dim(&self) -> usize {
            self.dim
        }
    }

    /// Cosine similarity; `None` on empty/mismatched dims.
    pub fn cosine(a: &[f32], b: &[f32]) -> Option<f32> {
        if a.is_empty() || b.is_empty() || a.len() != b.len() {
            return None;
        }
        let dot = a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let na = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let nb = b.iter().map(|x| x * x).sum::<f32>().sqrt();
        if na == 0.0 || nb == 0.0 {
            return Some(0.0);
        }
        Some(dot / (na * nb))
    }

    fn tokenize(text: &str) -> Vec<String> {
        let lower = text.to_lowercase();
        let mut toks: Vec<String> = Vec::new();
        let words: Vec<&str> = lower
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        for w in &words {
            toks.push((*w).to_string());
        }
        for pair in words.windows(2) {
            toks.push(format!("{} {}", pair[0], pair[1]));
        }
        let chars: Vec<char> = lower.chars().filter(|c| c.is_alphanumeric()).collect();
        for pair in chars.windows(2) {
            toks.push(pair.iter().collect());
        }
        toks
    }

    fn hash_dim(s: &str, dim: usize) -> usize {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
        (h as usize) % dim
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn similar_text_close_vectors() {
            let e = LocalEmbedder::new(super::super::EMBED_DIM);
            let a = e.embed("user prefers rust programming language").unwrap();
            let b = e.embed("user likes rust programming language").unwrap();
            let c = e.embed("banana smoothie recipe with ice").unwrap();
            assert!(cosine(&a, &b).unwrap() > cosine(&a, &c).unwrap());
        }

        #[test]
        fn deterministic_and_dim() {
            let e = LocalEmbedder::new(64);
            let a = e.embed("the quick brown fox").unwrap();
            let b = e.embed("the quick brown fox").unwrap();
            assert_eq!(a.len(), 64);
            assert_eq!(a, b);
        }

        #[test]
        fn empty_text_is_error() {
            let e = LocalEmbedder::new(64);
            assert!(e.embed("   ").is_err());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fragment_kind_roundtrip() {
        for k in [
            FragmentKind::Message,
            FragmentKind::ToolResult,
            FragmentKind::LongTerm,
            FragmentKind::Note,
        ] {
            let s = k.as_str();
            let back: FragmentKind = s.parse().unwrap();
            assert_eq!(k, back, "roundtrip failed for {k:?}");
        }
        assert!("bogus".parse::<FragmentKind>().is_err());
    }

    #[test]
    fn recall_query_defaults() {
        let q = RecallQuery::new("s", "x");
        assert_eq!(q.session, "s");
        assert_eq!(q.text, "x");
        assert_eq!(q.top_k, 8);
        assert!(q.kind.is_none());
        let q = q.with_kind(FragmentKind::Note);
        assert_eq!(q.kind, Some(FragmentKind::Note));
    }

    #[test]
    fn ensure_embedding_fills_vector_of_fixed_dim() {
        let mut f = ContextFragment::new("s", FragmentKind::Message, "rust programming");
        ensure_embedding(&mut f);
        let v = f.embedding.expect("embedding populated");
        assert_eq!(v.len(), EMBED_DIM);
    }

    #[test]
    fn ensure_embedding_leaves_empty_content_without_vector() {
        let mut f = ContextFragment::new("s", FragmentKind::Message, "   ");
        ensure_embedding(&mut f);
        assert!(f.embedding.is_none());
    }

    #[test]
    fn score_prefers_exact_key_then_vector() {
        let keyed =
            ContextFragment::new("s", FragmentKind::LongTerm, "unrelated").with_key("fact1");
        assert_eq!(keyword_score("fact1", &keyed), 1.0);

        let a = ContextFragment::new("s", FragmentKind::Message, "rust programming language");
        let b = ContextFragment::new("s", FragmentKind::Message, "banana smoothie recipe");
        let q = embed::LocalEmbedder::new(EMBED_DIM)
            .embed("rust programming")
            .unwrap();
        let sa = score_fragment("rust programming", Some(&q), &a);
        let sb = score_fragment("rust programming", Some(&q), &b);
        // Keyword-only scoring (no vectors on the fragments) still ranks the
        // substring hit above the unrelated text.
        assert!(score_fragment("rust", None, &a) > score_fragment("rust", None, &b));
        assert!(sa > sb);
    }

    #[test]
    fn rank_drops_zero_scores_and_truncates() {
        let a = ContextFragment::new("s", FragmentKind::Message, "alpha text");
        let b = ContextFragment::new("s", FragmentKind::Message, "beta text");
        let c = ContextFragment::new("s", FragmentKind::Message, "unrelated");
        let out = rank("alpha", None, vec![a, b, c], 8);
        assert_eq!(out.len(), 1);
        assert!(out[0].content.contains("alpha"));

        let many: Vec<ContextFragment> = (0..10)
            .map(|i| ContextFragment::new("s", FragmentKind::Message, format!("common {i}")))
            .collect();
        assert_eq!(rank("common", None, many, 3).len(), 3);
    }

    #[tokio::test]
    async fn memory_store_roundtrip_and_compact() {
        let store = MemoryContextStore::new();
        store
            .memorize(
                ContextFragment::new("s1", FragmentKind::Message, "hello world").with_key("k"),
            )
            .await
            .unwrap();
        let out = store
            .recall(&RecallQuery::new("s1", "hello"))
            .await
            .unwrap();
        assert!(out.iter().any(|f| f.content.contains("hello")));
        assert!(store.get_by_key("s1", "k").await.unwrap().is_some());
        assert!(store.get_by_key("s1", "missing").await.unwrap().is_none());

        // Empty query recalls nothing (abnormal path).
        assert!(store
            .recall(&RecallQuery::new("s1", ""))
            .await
            .unwrap()
            .is_empty());

        let c = store.compact("s1").await.unwrap();
        assert!(c.content.contains("hello world"));
        assert!(matches!(
            store.compact("ghost").await,
            Err(ContextError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn memory_store_isolates_sessions_and_kinds() {
        let store = MemoryContextStore::new();
        store
            .memorize(ContextFragment::new("a", FragmentKind::Message, "from a"))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new("b", FragmentKind::Message, "from b"))
            .await
            .unwrap();
        let out = store.recall(&RecallQuery::new("a", "from")).await.unwrap();
        assert!(out.iter().all(|f| f.session == "a"));

        let notes = store
            .recall(&RecallQuery::new("a", "from").with_kind(FragmentKind::Note))
            .await
            .unwrap();
        assert!(notes.is_empty());
    }

    // --- MemoryBackend / composite ("both") backend ---

    struct FailingStore;

    #[async_trait]
    impl ContextStore for FailingStore {
        async fn memorize(&self, _frag: ContextFragment) -> Result<(), ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn recall(&self, _query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
            Err(ContextError::NotFound(session.to_string()))
        }
        async fn get_by_key(
            &self,
            _session: &str,
            _key: &str,
        ) -> Result<Option<ContextFragment>, ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn list_session(
            &self,
            _session: &str,
            _top_k: usize,
        ) -> Result<Vec<ContextFragment>, ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
    }

    #[test]
    fn memory_backend_parses_and_roundtrips() {
        assert_eq!(MemoryBackend::default(), MemoryBackend::Cloud);
        for (raw, expected) in [
            ("cloud", MemoryBackend::Cloud),
            ("local", MemoryBackend::Local),
            ("both", MemoryBackend::Both),
            (" LOCAL ", MemoryBackend::Local),
        ] {
            let parsed: MemoryBackend = raw.parse().unwrap();
            assert_eq!(parsed, expected);
            assert_eq!(parsed.as_str(), expected.as_str());
            assert_eq!(parsed.to_string(), expected.as_str());
        }
        assert!("nope".parse::<MemoryBackend>().is_err());
    }

    #[test]
    fn merge_fragments_dedupes_by_id_keeping_the_freshest() {
        let mut older = ContextFragment::new("s", FragmentKind::Message, "old");
        older.id = "id-1".into();
        older.created_at = 1;
        let mut newer = ContextFragment::new("s", FragmentKind::Message, "new");
        newer.id = "id-1".into();
        newer.created_at = 2;
        let mut other = ContextFragment::new("s", FragmentKind::Note, "other");
        other.id = "id-2".into();

        let merged = merge_fragments(vec![older, other.clone()], vec![newer]);
        assert_eq!(merged.len(), 2);
        assert!(merged.iter().any(|f| f.id == "id-1" && f.content == "new"));
        assert!(merged
            .iter()
            .any(|f| f.id == "id-2" && f.content == "other"));
    }

    #[test]
    fn merge_fragments_falls_back_to_key_when_id_missing() {
        let a = ContextFragment::new("s", FragmentKind::LongTerm, "v1").with_key("k");
        let mut b = ContextFragment::new("s", FragmentKind::LongTerm, "v2").with_key("k");
        b.created_at = a.created_at + 5;
        let merged = merge_fragments(vec![a], vec![b]);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].content, "v2");
    }

    #[tokio::test]
    async fn composite_writes_to_both_backends() {
        let local = MemoryContextStore::new();
        let cloud = MemoryContextStore::new();
        let composite = CompositeContextStore::new(local.clone(), cloud.clone());

        composite
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "fact"))
            .await
            .unwrap();

        let q = RecallQuery::new("s", "fact");
        assert_eq!(local.recall(&q).await.unwrap().len(), 1);
        assert_eq!(cloud.recall(&q).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn composite_tolerates_one_failing_write() {
        let local = MemoryContextStore::new();
        let composite = CompositeContextStore::new(local.clone(), Arc::new(FailingStore));
        composite
            .memorize(ContextFragment::new("s", FragmentKind::Message, "kept"))
            .await
            .unwrap();
        assert_eq!(
            local
                .recall(&RecallQuery::new("s", "kept"))
                .await
                .unwrap()
                .len(),
            1
        );

        let cloud_only =
            CompositeContextStore::new(Arc::new(FailingStore), MemoryContextStore::new());
        cloud_only
            .memorize(ContextFragment::new("s", FragmentKind::Message, "kept"))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn composite_errors_when_every_write_fails() {
        let composite = CompositeContextStore::new(Arc::new(FailingStore), Arc::new(FailingStore));
        let res = composite
            .memorize(ContextFragment::new("s", FragmentKind::Message, "x"))
            .await;
        assert!(matches!(res, Err(ContextError::Storage(_))));
    }

    #[tokio::test]
    async fn composite_recall_merges_and_dedupes() {
        let local = MemoryContextStore::new();
        let cloud = MemoryContextStore::new();
        let composite = CompositeContextStore::new(local.clone(), cloud.clone());

        // Same fragment id written on both sides must appear once.
        let mut frag = ContextFragment::new("s", FragmentKind::LongTerm, "shared memory");
        frag.id = "shared".into();
        local.memorize(frag.clone()).await.unwrap();
        cloud.memorize(frag).await.unwrap();
        // Unique to each side (both match the query so both are recalled).
        local
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Note,
                "local memory",
            ))
            .await
            .unwrap();
        cloud
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Note,
                "cloud memory",
            ))
            .await
            .unwrap();

        let out = composite
            .recall(&RecallQuery {
                session: "s".into(),
                text: "memory".into(),
                top_k: 10,
                kind: None,
            })
            .await
            .unwrap();
        let shared_hits = out.iter().filter(|f| f.id == "shared").count();
        assert_eq!(shared_hits, 1, "duplicate fragments must be collapsed");
        assert!(out.iter().any(|f| f.content == "local memory"));
        assert!(out.iter().any(|f| f.content == "cloud memory"));
    }

    #[tokio::test]
    async fn composite_recall_survives_one_failing_backend() {
        let local = MemoryContextStore::new();
        local
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Message,
                "offline fact",
            ))
            .await
            .unwrap();
        let composite = CompositeContextStore::new(local, Arc::new(FailingStore));
        let out = composite
            .recall(&RecallQuery::new("s", "offline fact"))
            .await
            .unwrap();
        assert_eq!(out.len(), 1);

        let both_broken =
            CompositeContextStore::new(Arc::new(FailingStore), Arc::new(FailingStore));
        assert!(both_broken
            .recall(&RecallQuery::new("s", "x"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn composite_get_by_key_prefers_cloud_then_local() {
        let local = MemoryContextStore::new();
        let cloud = MemoryContextStore::new();
        local
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "from local").with_key("k"))
            .await
            .unwrap();
        let composite = CompositeContextStore::new(local.clone(), cloud.clone());
        assert_eq!(
            composite
                .get_by_key("s", "k")
                .await
                .unwrap()
                .unwrap()
                .content,
            "from local"
        );

        cloud
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "from cloud").with_key("k"))
            .await
            .unwrap();
        assert_eq!(
            composite
                .get_by_key("s", "k")
                .await
                .unwrap()
                .unwrap()
                .content,
            "from cloud"
        );
    }

    #[tokio::test]
    async fn memory_store_vector_recall_prefers_similar() {
        let store = MemoryContextStore::new();
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Message,
                "user prefers rust for systems programming",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::Message,
                "banana smoothie recipe with ice",
            ))
            .await
            .unwrap();
        let out = store
            .recall(&RecallQuery::new("s", "rust programming language"))
            .await
            .unwrap();
        assert!(!out.is_empty());
        assert!(out[0].content.contains("rust"));
        assert!(out[0].embedding.is_some());
    }
}
