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
    /// Optional dense vector for semantic recall (left empty in the local impl).
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

/// sled-backed implementation. Self-contained, no Postgres.
pub struct SledMemoStore {
    fragments: sled::Tree,
}

impl SledMemoStore {
    /// Open (or create) a memo store at `path`. Use `":memory:"` for a temp db.
    pub fn open(path: &Path) -> Result<Arc<Self>, MemoError> {
        let db = sled::open(path).map_err(|e| MemoError::Storage(e.to_string()))?;
        let fragments = db
            .open_tree("fragments")
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { fragments }))
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
        Ok(Arc::new(Self { fragments }))
    }
}

#[async_trait]
impl MemoStore for SledMemoStore {
    async fn memorize(&self, frag: ContextFragment) -> Result<(), MemoError> {
        let key = frag.id.as_bytes().to_vec();
        let value = serde_json::to_vec(&frag)?;
        self.fragments
            .insert(key, value)
            .map_err(|e| MemoError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, MemoError> {
        let q = query.text.to_lowercase();
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
            let mut score = 0.0f32;
            if frag.key.as_deref() == Some(query.text.as_str()) {
                score = 1.0;
            }
            let hay = frag.content.to_lowercase();
            if hay.contains(&q) {
                let hits = q
                    .split_whitespace()
                    .filter(|w| !w.is_empty() && hay.contains(*w))
                    .count() as f32;
                score = score.max(hits / (hay.len() as f32).max(1.0).log10());
            }
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
}
