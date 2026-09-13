//! `agent-memo` — the platform's unified **context memory** (memo).
//!
//! Design boundary (see `docs/adr/0004-memo-storage.md`):
//! * memo is the **only** source of conversational / long-term context.
//! * memo uses a **local / embedded** store (sled). It does **NOT** use Postgres.
//! * Postgres is used exclusively by `agent-cloud` for structured metadata
//!   (agents / sessions / runs). Context text never lives in Postgres.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

/// Kinds of context fragments memo can hold.
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

impl std::str::FromStr for FragmentKind {
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
    /// local [`embed::LocalEmbedder`] when a fragment is memorized without one.
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

/// Query used by [`MemoStore::recall`].
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
pub enum MemoError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("embedding error: {0}")]
    Embedding(String),
}

/// The unified context-memory contract. Everything that needs conversational
/// or long-term context goes through this trait.
#[async_trait]
pub trait MemoStore: Send + Sync {
    /// Persist a fragment.
    async fn memorize(&self, frag: ContextFragment) -> Result<(), MemoError>;
    /// Retrieve the most relevant fragments for a query.
    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, MemoError>;
    /// Merge a session's fragments into a single compact fragment.
    async fn compact(&self, session: &str) -> Result<ContextFragment, MemoError>;
    /// Lookup an explicit long-term memory by key (used by SDK `recall`).
    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, MemoError>;
}

/// sled-backed implementation. Self-contained, no Postgres. Uses a local
/// [`embed::LocalEmbedder`] to compute dense vectors for semantic (vector) recall.
pub struct SledMemoStore {
    fragments: sled::Tree,
    embedder: Arc<dyn embed::Embedder>,
}

impl SledMemoStore {
    /// Open (or create) a memo store at `path`. Use `":memory:"` for a temp db.
    pub fn open(path: &Path) -> Result<Arc<Self>, MemoError> {
        let db = sled::open(path).map_err(|e| MemoError::Storage(e.to_string()))?;
        let fragments = db
            .open_tree("fragments")
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        Ok(Arc::new(Self {
            fragments,
            embedder: Arc::new(embed::LocalEmbedder::new(64)),
        }))
    }

    /// In-memory variant (handy for tests and for the SDK default).
    pub fn memory() -> Result<Arc<Self>, MemoError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        let fragments = db
            .open_tree("fragments")
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        Ok(Arc::new(Self {
            fragments,
            embedder: Arc::new(embed::LocalEmbedder::new(64)),
        }))
    }
}

#[async_trait]
impl MemoStore for SledMemoStore {
    async fn memorize(&self, mut frag: ContextFragment) -> Result<(), MemoError> {
        // Compute a dense vector for semantic recall when one isn't supplied.
        if frag.embedding.is_none() && !frag.content.trim().is_empty() {
            if let Ok(v) = self.embedder.embed(&frag.content) {
                frag.embedding = Some(v);
            }
        }
        let key = frag.id.as_bytes().to_vec();
        let value = serde_json::to_vec(&frag)?;
        self.fragments
            .insert(key, value)
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, MemoError> {
        let q = query.text.to_lowercase();
        // Query vector for semantic (vector) recall; None when empty/unsupported.
        let q_emb = if q.trim().is_empty() {
            None
        } else {
            self.embedder.embed(&query.text).ok()
        };
        let mut scored: Vec<(f32, ContextFragment)> = Vec::new();
        for item in self.fragments.iter() {
            let (_k, v) = item.map_err(|e| MemoError::Storage(e.to_string()))?;
            let frag: ContextFragment = serde_json::from_slice(&v)?;
            if frag.session != query.session {
                continue;
            }
            if let Some(kind) = query.kind {
                if frag.kind != kind {
                    continue;
                }
            }
            // Keyword + length-weighted relevance (embeddings are optional).
            // An exact key match scores highest; otherwise we fall back to
            // substring/keyword matching against the content.
            let mut kw = 0.0f32;
            if frag.key.as_deref() == Some(query.text.as_str()) {
                kw = 1.0;
            }
            let hay = frag.content.to_lowercase();
            if hay.contains(&q) {
                let hits = q
                    .split_whitespace()
                    .filter(|w| !w.is_empty() && hay.contains(*w))
                    .count() as f32;
                kw = kw.max(hits / (hay.len() as f32).max(1.0).log10());
            }
            // Blend semantic (vector) similarity with the keyword score.
            // Pure keyword when a vector isn't available on either side.
            let score = match (&q_emb, &frag.embedding) {
                (Some(a), Some(b)) => match embed::cosine(a, b) {
                    Some(v) => 0.7 * v + 0.3 * kw,
                    None => kw,
                },
                _ => kw,
            };
            if score > 0.0 {
                scored.push((score, frag));
            }
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(query.top_k);
        Ok(scored.into_iter().map(|(_, f)| f).collect())
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, MemoError> {
        let mut parts: Vec<String> = Vec::new();
        for item in self.fragments.iter() {
            let (_k, v) = item.map_err(|e| MemoError::Storage(e.to_string()))?;
            let frag: ContextFragment = serde_json::from_slice(&v)?;
            if frag.session == session {
                parts.push(format!("[{}] {}", frag.kind.as_str(), frag.content));
            }
        }
        if parts.is_empty() {
            return Err(MemoError::NotFound(session.to_string()));
        }
        let merged = ContextFragment::new(session, FragmentKind::Note, parts.join("\n---\n"))
            .with_key(format!("__compact__{}", session));
        self.memorize(merged.clone()).await?;
        Ok(merged)
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, MemoError> {
        for item in self.fragments.iter() {
            let (_k, v) = item.map_err(|e| MemoError::Storage(e.to_string()))?;
            let frag: ContextFragment = serde_json::from_slice(&v)?;
            if frag.session == session && frag.key.as_deref() == Some(key) {
                return Ok(Some(frag));
            }
        }
        Ok(None)
    }
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Local lightweight embedding + cosine similarity for vector recall.
///
/// Uses the hashing trick (word 1~2-grams + char 2-grams via FNV-1a) into a
/// fixed-dim vector, TF-normalized then L2-normalized — the same approach as
/// the `memo` product's `memo-embed::LocalEmbedder`. Zero external/ML deps.
pub mod embed {
    use super::MemoError;
    use std::collections::HashMap;

    /// Text embedding seam for the local memo store.
    pub trait Embedder: Send + Sync {
        fn embed(&self, text: &str) -> Result<Vec<f32>, MemoError>;
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

        fn vectorize(&self, text: &str) -> Result<Vec<f32>, MemoError> {
            let toks = tokenize(text);
            if toks.is_empty() {
                return Err(MemoError::Embedding("empty embedding text".into()));
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
                return Err(MemoError::Embedding("zero-magnitude vector".into()));
            }
            for v in vec.iter_mut() {
                *v /= norm;
            }
            Ok(vec)
        }
    }

    impl Embedder for LocalEmbedder {
        fn embed(&self, text: &str) -> Result<Vec<f32>, MemoError> {
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
            let e = LocalEmbedder::new(64);
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

    #[tokio::test]
    async fn memorize_recall_roundtrip() {
        let store = SledMemoStore::memory().unwrap();
        let f =
            ContextFragment::new("s1", FragmentKind::Message, "the sky is blue").with_key("fact1");
        store.memorize(f).await.unwrap();
        let out = store.recall(&RecallQuery::new("s1", "sky")).await.unwrap();
        assert!(out.iter().any(|f| f.content.contains("sky")));

        let by_key = store.get_by_key("s1", "fact1").await.unwrap();
        assert!(by_key.is_some());
    }

    #[tokio::test]
    async fn compact_merges_session() {
        let store = SledMemoStore::memory().unwrap();
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
    }

    #[tokio::test]
    async fn vector_recall_prefers_similar() {
        let store = SledMemoStore::memory().unwrap();
        // Memorize without an explicit embedding; the store computes it.
        let rust = ContextFragment::new("s3", FragmentKind::Message, "user prefers rust for systems programming");
        let food = ContextFragment::new("s3", FragmentKind::Message, "banana smoothie recipe with ice");
        store.memorize(rust).await.unwrap();
        store.memorize(food).await.unwrap();

        let frags = store
            .recall(&RecallQuery::new("s3", "rust programming language"))
            .await
            .unwrap();
        assert!(!frags.is_empty());
        // The semantically closer rust memory must rank first.
        assert!(frags[0].content.contains("rust"));
        // Fragments are persisted with their dense vectors.
        assert!(frags[0].embedding.is_some());
    }
}
