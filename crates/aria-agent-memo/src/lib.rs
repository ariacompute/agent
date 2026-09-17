//! `agent-memo` — the **on-device / embedded** context store.
//!
//! The contract ([`ContextStore`], [`ContextFragment`], [`RecallQuery`], the
//! local [`embed`] module) now lives in `aria-agent-core::context` so the cloud
//! can ship a Postgres + pgvector implementation without depending on this
//! crate (see `docs/adr/0010-context-storage-pgvector.md`).
//!
//! This crate keeps the local/embedded backend used by the native SDK
//! (Swift / Kotlin via UniFFI): a sled-backed store that runs in-process with
//! no network and no Postgres.

use agent_core::context::embed::Embedder as _;
use agent_core::context::{
    ensure_embedding, rank, ContextError, ContextFragment, ContextStore, FragmentKind, RecallQuery,
    EMBED_DIM,
};
use async_trait::async_trait;
use std::path::Path;
use std::sync::Arc;

pub use agent_core::context::{
    embed, now_ms, ContextError as MemoError, EMBED_DIM as MEMO_EMBED_DIM,
};

/// sled-backed implementation of the shared context contract. Self-contained,
/// no Postgres, no network — the store the on-device SDK embeds.
pub struct SledContextStore {
    fragments: sled::Tree,
}

impl SledContextStore {
    /// Open (or create) a context store at `path`. Use `":memory:"` semantics
    /// via [`SledContextStore::memory`] for a temp db.
    pub fn open(path: &Path) -> Result<Arc<Self>, ContextError> {
        let db = sled::open(path).map_err(|e| ContextError::Storage(e.to_string()))?;
        let fragments = db
            .open_tree("fragments")
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { fragments }))
    }

    /// In-memory variant (handy for tests and for the SDK default).
    pub fn memory() -> Result<Arc<Self>, ContextError> {
        let db = sled::Config::new()
            .temporary(true)
            .open()
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        let fragments = db
            .open_tree("fragments")
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        Ok(Arc::new(Self { fragments }))
    }

    fn all(&self) -> Result<Vec<ContextFragment>, ContextError> {
        let mut out = Vec::new();
        for item in self.fragments.iter() {
            let (_k, v) = item.map_err(|e| ContextError::Storage(e.to_string()))?;
            out.push(serde_json::from_slice(&v)?);
        }
        Ok(out)
    }
}

#[async_trait]
impl ContextStore for SledContextStore {
    async fn memorize(&self, mut frag: ContextFragment) -> Result<(), ContextError> {
        // Compute a dense vector for semantic recall when one isn't supplied.
        ensure_embedding(&mut frag);
        let key = frag.id.as_bytes().to_vec();
        let value = serde_json::to_vec(&frag)?;
        self.fragments
            .insert(key, value)
            .map_err(|e| ContextError::Storage(e.to_string()))?;
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        if query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let q_emb = embed::LocalEmbedder::new(EMBED_DIM).embed(&query.text).ok();
        let candidates: Vec<ContextFragment> = self
            .all()?
            .into_iter()
            .filter(|f| f.session == query.session)
            .filter(|f| query.kind.map(|k| f.kind == k).unwrap_or(true))
            .collect();
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
        Ok(self
            .all()?
            .into_iter()
            .find(|f| f.session == session && f.key.as_deref() == Some(key)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Arc<SledContextStore> {
        SledContextStore::memory().unwrap()
    }

    #[tokio::test]
    async fn memorize_recall_roundtrip() {
        let store = store();
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
    }

    #[tokio::test]
    async fn vector_recall_prefers_similar() {
        let store = store();
        // Memorize without an explicit embedding; the store computes it.
        let rust = ContextFragment::new(
            "s3",
            FragmentKind::Message,
            "user prefers rust for systems programming",
        );
        let food = ContextFragment::new(
            "s3",
            FragmentKind::Message,
            "banana smoothie recipe with ice",
        );
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

    #[tokio::test]
    async fn recall_filters_by_kind() {
        let store = store();
        store
            .memorize(ContextFragment::new("s", FragmentKind::Message, "alpha"))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new("s", FragmentKind::Note, "beta"))
            .await
            .unwrap();
        let msgs = store
            .recall(&RecallQuery::new("s", "alpha").with_kind(FragmentKind::Message))
            .await
            .unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].kind, FragmentKind::Message);
        assert!(msgs[0].content.contains("alpha"));
    }

    #[tokio::test]
    async fn recall_respects_session() {
        let store = store();
        store
            .memorize(ContextFragment::new("a", FragmentKind::Message, "from a"))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new("b", FragmentKind::Message, "from b"))
            .await
            .unwrap();
        let out = store.recall(&RecallQuery::new("a", "from")).await.unwrap();
        assert!(!out.is_empty());
        assert!(out.iter().all(|f| f.session == "a"));
        assert!(out.iter().any(|f| f.content == "from a"));
        assert!(!out.iter().any(|f| f.content == "from b"));
    }

    #[tokio::test]
    async fn recall_empty_query_returns_empty() {
        let store = store();
        store
            .memorize(ContextFragment::new("s", FragmentKind::Message, "hello"))
            .await
            .unwrap();
        let out = store.recall(&RecallQuery::new("s", "")).await.unwrap();
        assert!(out.is_empty());
    }

    #[tokio::test]
    async fn get_by_key_roundtrip_and_missing() {
        let store = store();
        let f = ContextFragment::new("s", FragmentKind::LongTerm, "fact").with_key("k1");
        store.memorize(f).await.unwrap();
        let got = store.get_by_key("s", "k1").await.unwrap();
        assert!(got.is_some());
        assert_eq!(got.unwrap().content, "fact");
        assert!(store.get_by_key("s", "nope").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn compact_missing_session_is_not_found() {
        let store = store();
        assert!(matches!(
            store.compact("ghost").await,
            Err(ContextError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn memorize_populates_embedding() {
        let store = store();
        let f = ContextFragment::new("s", FragmentKind::Message, "rust programming").with_key("ke");
        store.memorize(f).await.unwrap();
        let got = store.get_by_key("s", "ke").await.unwrap().unwrap();
        assert!(got.embedding.is_some());
        assert_eq!(got.embedding.unwrap().len(), EMBED_DIM);
    }

    #[tokio::test]
    async fn recall_exact_key_match_ranks_first() {
        let store = store();
        store
            .memorize(
                ContextFragment::new("s", FragmentKind::Message, "unrelated content")
                    .with_key("fact1"),
            )
            .await
            .unwrap();
        let out = store.recall(&RecallQuery::new("s", "fact1")).await.unwrap();
        assert!(!out.is_empty());
        assert_eq!(out[0].key.as_deref(), Some("fact1"));
    }
}
