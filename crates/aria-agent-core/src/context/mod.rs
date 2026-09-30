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

// ---------------------------------------------------------------------------
// Multi-relational memory plane (Jev-Mem inspired: semantic/temporal/causal/entity)
// ---------------------------------------------------------------------------

/// The four relational graph views. Mirrors the `memo` crate's `RelationKind`
/// so the two workspaces agree on the wire shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RelationKind {
    /// Conceptual / meaning association.
    Semantic,
    /// Temporal ordering or co-occurrence.
    Temporal,
    /// Causal chain.
    Causal,
    /// Same entity / object aggregation.
    Entity,
}

impl RelationKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RelationKind::Semantic => "semantic",
            RelationKind::Temporal => "temporal",
            RelationKind::Causal => "causal",
            RelationKind::Entity => "entity",
        }
    }
}

impl std::str::FromStr for RelationKind {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "semantic" => Ok(RelationKind::Semantic),
            "temporal" => Ok(RelationKind::Temporal),
            "causal" => Ok(RelationKind::Causal),
            "entity" => Ok(RelationKind::Entity),
            other => Err(format!("unknown relation_kind: {other}")),
        }
    }
}

/// A directed relation edge between two fragments, scoped to a session. Stored
/// in its own table so `ContextFragment` is never mutated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Relation {
    pub session: String,
    pub from_id: String,
    pub to_id: String,
    pub kind: RelationKind,
    /// Edge confidence in [0, 1].
    pub score: f32,
    /// Provenance description (e.g. "local:semantic" or "llm:causal").
    pub provenance: String,
    /// Unix epoch milliseconds.
    pub created_at: i64,
}

impl Relation {
    pub fn validate(&self) -> Result<(), ContextError> {
        if self.from_id == self.to_id {
            return Err(ContextError::NotFound(format!(
                "relation self-loop rejected: {}",
                self.from_id
            )));
        }
        if !(0.0..=1.0).contains(&self.score) {
            return Err(ContextError::Storage(format!(
                "relation score {} out of [0,1]",
                self.score
            )));
        }
        if self.provenance.trim().is_empty() {
            return Err(ContextError::Storage("empty relation provenance".into()));
        }
        Ok(())
    }
}

/// A bounded graph retrieval query (mirrors Jev-Mem's Retrieve->Assess->Expand).
#[derive(Debug, Clone)]
pub struct GraphRetrieveQuery {
    pub session: String,
    /// Seed fragment ids to start expansion from.
    pub seeds: Vec<String>,
    /// Relation views to traverse; empty means "all four views".
    pub views: Vec<RelationKind>,
    /// Maximum number of distinct fragments to visit (budget).
    pub budget: usize,
    /// Maximum traversal depth.
    pub max_hops: usize,
    /// Final number of scored fragments to return.
    pub top_k: usize,
}

impl GraphRetrieveQuery {
    pub fn validate(&self) -> Result<(), ContextError> {
        if self.seeds.is_empty() {
            return Err(ContextError::Storage("graph query needs >= 1 seed".into()));
        }
        if self.budget == 0 {
            return Err(ContextError::Storage("graph budget must be > 0".into()));
        }
        if self.max_hops == 0 {
            return Err(ContextError::Storage("graph max_hops must be > 0".into()));
        }
        if self.top_k == 0 {
            return Err(ContextError::Storage("graph top_k must be > 0".into()));
        }
        Ok(())
    }
}

/// Inspectable decision trace returned alongside a graph retrieval, mirroring
/// Jev-Mem's typed/transparent decisions.
#[derive(Debug, Clone)]
pub struct RetrieveTrace {
    pub views: Vec<RelationKind>,
    pub budget: usize,
    pub stop_reason: String,
    pub hits: usize,
}

/// A fragment plus its graph-evidence score.
#[derive(Debug, Clone)]
pub struct ScoredFragment {
    pub fragment: ContextFragment,
    pub score: f32,
}

/// Bundled result of a bounded graph expansion.
#[derive(Debug, Clone)]
pub struct GraphRetrieveResult {
    pub items: Vec<ScoredFragment>,
    pub trace: RetrieveTrace,
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

    // --- Multi-relational memory plane (Jev-Mem inspired) ---

    /// Persist a relation edge (session-scoped). Returns `NotFound` if either
    /// endpoint fragment is missing, `Storage` on a self-loop/invalid edge.
    async fn relate(&self, rel: Relation) -> Result<(), ContextError>;

    /// List relations for a session, optionally filtered by `from`/`to`/`kind`.
    /// `top_k` caps the returned edges (oldest skipped).
    async fn get_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
        top_k: usize,
    ) -> Result<Vec<Relation>, ContextError>;

    /// Delete relations for a session matching the given filters. Returns the
    /// number removed. At least one of `from`/`to`/`kind` must be set.
    async fn delete_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
    ) -> Result<usize, ContextError>;

    /// Bounded graph expansion from `query.seeds` across `query.views`. Returns
    /// the reached scored fragments plus an inspectable [`RetrieveTrace`].
    async fn expand(&self, query: &GraphRetrieveQuery)
        -> Result<GraphRetrieveResult, ContextError>;
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

/// Token Jaccard overlap of two contents (used by the entity/causal heuristics).
pub(crate) fn token_jaccard(a: &str, b: &str) -> f32 {
    fn toks(s: &str) -> std::collections::HashSet<String> {
        s.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .filter(|w| w.len() >= 2)
            .map(|w| w.to_string())
            .collect()
    }
    let ta = toks(a);
    let tb = toks(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    inter / union
}

/// Generic bounded graph traversal (BFS with cycle avoidance) shared by every
/// backend. `neighbor_fn` returns outgoing edges `(to_id, score, kind)` for a
/// node; the caller filters by `views`, caps visited nodes at `budget`, and
/// decays the accumulated score by 0.9 per hop. The returned `stop` reason is
/// human-readable ("budget" / "exhausted").
pub fn graph_bfs(
    seeds: &[String],
    views: &[RelationKind],
    budget: usize,
    max_hops: usize,
    mut neighbor_fn: impl FnMut(&str) -> Vec<(String, f32, RelationKind)>,
) -> (Vec<(String, f32)>, String) {
    use std::collections::{HashMap, HashSet, VecDeque};

    let view_set: Option<&[RelationKind]> = if views.is_empty() { None } else { Some(views) };
    let mut best: HashMap<String, f32> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, usize, f32)> = VecDeque::new();

    for s in seeds {
        if visited.insert(s.clone()) {
            best.insert(s.clone(), 1.0);
            queue.push_back((s.clone(), 0, 1.0));
        }
    }

    let mut stop = "exhausted".to_string();
    while let Some((node, hop, acc)) = queue.pop_front() {
        if hop >= max_hops {
            continue;
        }
        let edges = neighbor_fn(&node);
        for (nid, escore, kind) in edges {
            if let Some(vs) = view_set {
                if !vs.contains(&kind) {
                    continue;
                }
            }
            if visited.contains(&nid) {
                continue;
            }
            if visited.len() >= budget {
                stop = "budget".to_string();
                break;
            }
            let nacc = acc * escore * 0.9;
            visited.insert(nid.clone());
            let e = best.entry(nid.clone()).or_insert(0.0);
            if nacc > *e {
                *e = nacc;
            }
            queue.push_back((nid, hop + 1, nacc));
        }
        if stop == "budget" {
            break;
        }
    }

    let mut out: Vec<(String, f32)> = best.into_iter().collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    (out, stop)
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

    async fn relate(&self, rel: Relation) -> Result<(), ContextError> {
        let local = self.local.relate(rel.clone()).await;
        let cloud = self.cloud.relate(rel).await;
        match (local, cloud) {
            (Ok(()), Ok(())) => Ok(()),
            (Ok(()), Err(e)) => {
                tracing::warn!("context: cloud relate failed, kept local edge: {e}");
                Ok(())
            }
            (Err(e), Ok(())) => {
                tracing::warn!("context: local relate failed, kept cloud edge: {e}");
                Ok(())
            }
            (Err(e), Err(_)) => Err(e),
        }
    }

    async fn get_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
        top_k: usize,
    ) -> Result<Vec<Relation>, ContextError> {
        let local = self
            .local
            .get_relations(session, from, to, kind, top_k)
            .await;
        let cloud = self
            .cloud
            .get_relations(session, from, to, kind, top_k)
            .await;
        let (local, cloud) = match (local, cloud) {
            (Ok(l), Ok(c)) => (l, c),
            (Ok(l), Err(e)) => {
                tracing::warn!("context: cloud get_relations failed, using local: {e}");
                (l, Vec::new())
            }
            (Err(e), Ok(c)) => {
                tracing::warn!("context: local get_relations failed, using cloud: {e}");
                (Vec::new(), c)
            }
            (Err(e), Err(_)) => return Err(e),
        };
        // Merge by (session, from, to, kind), keeping the freshest copy.
        let mut index: HashMap<(String, String, String, RelationKind), usize> = HashMap::new();
        let mut out: Vec<Relation> = Vec::new();
        for r in local.into_iter().chain(cloud) {
            let key = (
                r.session.clone(),
                r.from_id.clone(),
                r.to_id.clone(),
                r.kind,
            );
            match index.get(&key).copied() {
                Some(i) if r.created_at > out[i].created_at => out[i] = r,
                Some(_) => {}
                None => {
                    let i = out.len();
                    index.insert(key, i);
                    out.push(r);
                }
            }
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        out.truncate(top_k);
        Ok(out)
    }

    async fn delete_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<RelationKind>,
    ) -> Result<usize, ContextError> {
        let local = self.local.delete_relations(session, from, to, kind).await;
        let cloud = self.cloud.delete_relations(session, from, to, kind).await;
        let (local, cloud) = match (local, cloud) {
            (Ok(l), Ok(c)) => (l, c),
            (Ok(l), Err(e)) => {
                tracing::warn!("context: cloud delete_relations failed: {e}");
                (l, 0)
            }
            (Err(e), Ok(c)) => {
                tracing::warn!("context: local delete_relations failed: {e}");
                (0, c)
            }
            (Err(e), Err(_)) => return Err(e),
        };
        Ok(local + cloud)
    }

    async fn expand(
        &self,
        query: &GraphRetrieveQuery,
    ) -> Result<GraphRetrieveResult, ContextError> {
        // Gather edges from both sides, merge by (session, from, to, kind) keeping
        // the freshest, then run a single bounded traversal.
        let all = self
            .get_relations(&query.session, None, None, None, usize::MAX)
            .await?;
        let mut edges: HashMap<String, Vec<(String, f32, RelationKind)>> = HashMap::new();
        let mut frags: HashMap<String, ContextFragment> = HashMap::new();
        for r in all {
            edges
                .entry(r.from_id.clone())
                .or_default()
                .push((r.to_id.clone(), r.score, r.kind));
        }
        for f in self
            .local
            .list_session(&query.session, usize::MAX)
            .await
            .unwrap_or_default()
            .into_iter()
            .chain(
                self.cloud
                    .list_session(&query.session, usize::MAX)
                    .await
                    .unwrap_or_default(),
            )
        {
            frags.insert(f.id.clone(), f);
        }
        let (reached, stop) = graph_bfs(
            &query.seeds,
            &query.views,
            query.budget,
            query.max_hops,
            |n| edges.get(n).cloned().unwrap_or_default(),
        );
        let mut items: Vec<ScoredFragment> = Vec::new();
        for (id, score) in reached.into_iter().take(query.top_k) {
            if let Some(f) = frags.get(&id) {
                items.push(ScoredFragment {
                    fragment: f.clone(),
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

/// Tunables for the relation controller.
#[derive(Debug, Clone)]
pub struct RelationConfig {
    /// How many existing fragments to evaluate as relation candidates per write.
    pub candidate_top_k: usize,
    /// Minimum score to accept an inferred relation edge.
    pub relation_threshold: f32,
    /// Cap on edges created in a single write→connect pass.
    pub max_relations_per_write: usize,
}

impl Default for RelationConfig {
    fn default() -> Self {
        Self {
            candidate_top_k: 10,
            relation_threshold: 0.6,
            max_relations_per_write: 8,
        }
    }
}

/// Pluggable relation scorer. The default [`LocalRelationScorer`] runs fully
/// offline; swap in an LLM-backed implementation without touching the controller.
pub trait RelationScorer: Send + Sync {
    fn score(&self, a: &ContextFragment, b: &ContextFragment, kind: RelationKind) -> f32;
}

/// Default offline heuristic scorer mirroring the four relation views.
pub struct LocalRelationScorer;

impl RelationScorer for LocalRelationScorer {
    fn score(&self, a: &ContextFragment, b: &ContextFragment, kind: RelationKind) -> f32 {
        match kind {
            RelationKind::Semantic => match (&a.embedding, &b.embedding) {
                (Some(x), Some(y)) => embed::cosine(x, y).unwrap_or(0.0),
                _ => token_jaccard(&a.content, &b.content),
            },
            RelationKind::Temporal => {
                if a.created_at <= b.created_at {
                    1.0
                } else {
                    0.0
                }
            }
            RelationKind::Entity => token_jaccard(&a.content, &b.content),
            RelationKind::Causal => {
                let adj = if a.created_at <= b.created_at {
                    0.5
                } else {
                    0.0
                };
                let overlap = token_jaccard(&a.content, &b.content);
                (adj + 0.5 * overlap).min(1.0)
            }
        }
    }
}

/// Local control plane: write→connect relation inference and
/// retrieve→assess→expand bounded graph traversal.
pub struct ContextController {
    store: Arc<dyn ContextStore>,
    #[allow(dead_code)]
    embedder: Arc<dyn embed::Embedder>,
    scorer: Arc<dyn RelationScorer>,
    cfg: RelationConfig,
}

impl ContextController {
    pub fn new(
        store: Arc<dyn ContextStore>,
        embedder: Arc<dyn embed::Embedder>,
        scorer: Arc<dyn RelationScorer>,
        cfg: RelationConfig,
    ) -> Self {
        Self {
            store,
            embedder,
            scorer,
            cfg,
        }
    }

    /// Convenience constructor with the offline default scorer and config.
    pub fn with_defaults(store: Arc<dyn ContextStore>, embedder: Arc<dyn embed::Embedder>) -> Self {
        Self::new(
            store,
            embedder,
            Arc::new(LocalRelationScorer),
            RelationConfig::default(),
        )
    }

    /// Write→connect: after `frag` is persisted, pick bounded candidates and
    /// infer four-view relations, persisting edges above the threshold.
    /// Returns the number of edges created.
    pub async fn connect(&self, frag: &ContextFragment) -> Result<usize, ContextError> {
        // The source fragment must already be persisted (write→connect ordering).
        let exists = self
            .store
            .recall(&RecallQuery::new(&frag.session, &frag.content))
            .await?
            .iter()
            .any(|f| f.id == frag.id);
        if !exists {
            return Err(ContextError::NotFound(frag.id.clone()));
        }
        let mut rq = RecallQuery::new(&frag.session, &frag.content);
        rq.top_k = self.cfg.candidate_top_k + 1;
        let candidates = self.store.recall(&rq).await?;
        let candidates: Vec<ContextFragment> =
            candidates.into_iter().filter(|f| f.id != frag.id).collect();
        let mut added = 0;
        for cand in candidates.iter().take(self.cfg.max_relations_per_write) {
            for kind in [
                RelationKind::Semantic,
                RelationKind::Temporal,
                RelationKind::Causal,
                RelationKind::Entity,
            ] {
                let sc = self.scorer.score(frag, cand, kind);
                if sc < self.cfg.relation_threshold {
                    continue;
                }
                let now = now_ms();
                let forward = kind == RelationKind::Temporal && frag.created_at > cand.created_at;
                if !forward {
                    self.store
                        .relate(Relation {
                            session: frag.session.clone(),
                            from_id: frag.id.clone(),
                            to_id: cand.id.clone(),
                            kind,
                            score: sc,
                            provenance: "local".into(),
                            created_at: now,
                        })
                        .await?;
                    added += 1;
                }
                if kind != RelationKind::Temporal {
                    self.store
                        .relate(Relation {
                            session: frag.session.clone(),
                            from_id: cand.id.clone(),
                            to_id: frag.id.clone(),
                            kind,
                            score: sc,
                            provenance: "local".into(),
                            created_at: now,
                        })
                        .await?;
                    added += 1;
                }
            }
        }
        Ok(added)
    }

    /// Retrieve→assess→expand: hybrid recall seeds anchors, then bounded graph
    /// expansion across `views` returns scored fragments plus an inspectable trace.
    pub async fn retrieve(
        &self,
        query: &RecallQuery,
        views: Vec<RelationKind>,
        budget: usize,
        max_hops: usize,
        top_k: usize,
    ) -> Result<GraphRetrieveResult, ContextError> {
        if budget == 0 || max_hops == 0 || top_k == 0 {
            return Err(ContextError::Storage(
                "retrieve budget/max_hops/top_k must be > 0".into(),
            ));
        }
        let seeds: Vec<String> = self
            .store
            .recall(query)
            .await?
            .into_iter()
            .map(|f| f.id)
            .collect();
        if seeds.is_empty() {
            return Ok(GraphRetrieveResult {
                items: Vec::new(),
                trace: RetrieveTrace {
                    views,
                    budget,
                    stop_reason: "no-seeds".into(),
                    hits: 0,
                },
            });
        }
        let q = GraphRetrieveQuery {
            session: query.session.clone(),
            seeds,
            views,
            budget,
            max_hops,
            top_k,
        };
        self.store.expand(&q).await
    }
}

/// In-process [`ContextStore`] used by tests and by the SDK default.
///
/// Kept in `agent-core` so core's tests need no on-device database (and no
/// dependency on the embedded store crate).
pub struct MemoryContextStore {
    inner: RwLock<HashMap<String, ContextFragment>>,
    relations: RwLock<Vec<Relation>>,
}

impl MemoryContextStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: RwLock::new(HashMap::new()),
            relations: RwLock::new(Vec::new()),
        })
    }
}

impl Default for MemoryContextStore {
    fn default() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
            relations: RwLock::new(Vec::new()),
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

    async fn relate(&self, rel: Relation) -> Result<(), ContextError> {
        rel.validate()?;
        {
            let g = self
                .inner
                .read()
                .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
            if !g.contains_key(&rel.from_id) {
                return Err(ContextError::NotFound(rel.from_id.clone()));
            }
            if !g.contains_key(&rel.to_id) {
                return Err(ContextError::NotFound(rel.to_id.clone()));
            }
        }
        let mut rels = self
            .relations
            .write()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        if let Some(existing) = rels.iter_mut().find(|r| {
            r.session == rel.session
                && r.from_id == rel.from_id
                && r.to_id == rel.to_id
                && r.kind == rel.kind
        }) {
            existing.score = rel.score;
            existing.provenance = rel.provenance;
            existing.created_at = rel.created_at;
        } else {
            rels.push(rel);
        }
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
        let rels = self
            .relations
            .read()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        let mut out: Vec<Relation> = rels
            .iter()
            .filter(|r| r.session == session)
            .filter(|r| from.map(|f| r.from_id == f).unwrap_or(true))
            .filter(|r| to.map(|t| r.to_id == t).unwrap_or(true))
            .filter(|r| kind.map(|k| k == r.kind).unwrap_or(true))
            .cloned()
            .collect();
        out.sort_by_key(|r| std::cmp::Reverse(r.created_at));
        out.truncate(top_k);
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
        let mut rels = self
            .relations
            .write()
            .map_err(|e| ContextError::Storage(format!("memory store lock poisoned: {e}")))?;
        let before = rels.len();
        rels.retain(|r| {
            // A provided filter must match; an absent filter is ignored. The
            // top-level guard above already guarantees at least one is present.
            let matches = r.session == session
                && from.map(|f| r.from_id == f).unwrap_or(true)
                && to.map(|t| r.to_id == t).unwrap_or(true)
                && kind.map(|k| k == r.kind).unwrap_or(true);
            !matches
        });
        Ok(before - rels.len())
    }

    async fn expand(
        &self,
        query: &GraphRetrieveQuery,
    ) -> Result<GraphRetrieveResult, ContextError> {
        query.validate()?;
        let (rels_snapshot, frags_snapshot) =
            {
                let rels = self.relations.read().map_err(|e| {
                    ContextError::Storage(format!("memory store lock poisoned: {e}"))
                })?;
                let frags = self.inner.read().map_err(|e| {
                    ContextError::Storage(format!("memory store lock poisoned: {e}"))
                })?;
                (rels.clone(), frags.clone())
            };
        let mut edges: HashMap<String, Vec<(String, f32, RelationKind)>> = HashMap::new();
        for r in rels_snapshot.iter() {
            if r.session != query.session {
                continue;
            }
            edges
                .entry(r.from_id.clone())
                .or_default()
                .push((r.to_id.clone(), r.score, r.kind));
        }
        let (reached, stop) = graph_bfs(
            &query.seeds,
            &query.views,
            query.budget,
            query.max_hops,
            |n| edges.get(n).cloned().unwrap_or_default(),
        );
        let mut items: Vec<ScoredFragment> = Vec::new();
        for (id, score) in reached.into_iter().take(query.top_k) {
            if let Some(f) = frags_snapshot.get(&id) {
                items.push(ScoredFragment {
                    fragment: f.clone(),
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
        async fn relate(&self, _rel: Relation) -> Result<(), ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn get_relations(
            &self,
            _session: &str,
            _from: Option<&str>,
            _to: Option<&str>,
            _kind: Option<RelationKind>,
            _top_k: usize,
        ) -> Result<Vec<Relation>, ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn delete_relations(
            &self,
            _session: &str,
            _from: Option<&str>,
            _to: Option<&str>,
            _kind: Option<RelationKind>,
        ) -> Result<usize, ContextError> {
            Err(ContextError::Storage("boom".into()))
        }
        async fn expand(
            &self,
            _query: &GraphRetrieveQuery,
        ) -> Result<GraphRetrieveResult, ContextError> {
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

    // --- Multi-relational memory plane ---

    #[test]
    fn relation_kind_roundtrip() {
        for k in [
            RelationKind::Semantic,
            RelationKind::Temporal,
            RelationKind::Causal,
            RelationKind::Entity,
        ] {
            let s = k.as_str();
            let back: RelationKind = s.parse().unwrap();
            assert_eq!(k, back);
        }
        assert!("bogus".parse::<RelationKind>().is_err());
    }

    #[tokio::test]
    async fn memory_store_relation_crud_and_expand() {
        let store = MemoryContextStore::new();
        store
            .memorize(ContextFragment::new(
                "s1",
                FragmentKind::LongTerm,
                "user likes rust programming",
            ))
            .await
            .unwrap();
        store
            .memorize(ContextFragment::new(
                "s1",
                FragmentKind::LongTerm,
                "rust is used for systems programming",
            ))
            .await
            .unwrap();
        // Capture the generated ids.
        let frags = store.list_session("s1", 10).await.unwrap();
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
                session: "s1".into(),
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
                session: "s1".into(),
                from_id: a.id.clone(),
                to_id: b.id.clone(),
                kind: RelationKind::Semantic,
                score: 0.9,
                provenance: "local".into(),
                created_at: 1,
            })
            .await
            .unwrap();
        let all = store
            .get_relations("s1", None, None, None, 10)
            .await
            .unwrap();
        assert_eq!(all.len(), 1, "all relations: {:#?}", all);
        let rels = store
            .get_relations("s1", Some(&a.id), None, None, 10)
            .await
            .unwrap();
        assert_eq!(rels.len(), 1, "relations for a.id={}: {:#?}", a.id, rels);

        // Expand a -> b.
        let q = GraphRetrieveQuery {
            session: "s1".into(),
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

        // Wrong session returns nothing.
        assert!(store
            .get_relations("other", None, None, None, 10)
            .await
            .unwrap()
            .is_empty());

        // Delete by from.
        let removed = store
            .delete_relations("s1", Some(&a.id), None, None)
            .await
            .unwrap();
        assert_eq!(removed, 1);
        // Empty filter rejected.
        assert!(store
            .delete_relations("s1", None, None, None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn context_controller_write_connect_and_retrieve() {
        let store = MemoryContextStore::new();
        let a = ContextFragment::new(
            "s1",
            FragmentKind::LongTerm,
            "user likes rust programming language",
        );
        let b = ContextFragment::new(
            "s1",
            FragmentKind::LongTerm,
            "user likes rust systems programming",
        );
        store.memorize(a.clone()).await.unwrap();
        store.memorize(b.clone()).await.unwrap();

        let ctrl = ContextController::with_defaults(
            store.clone(),
            Arc::new(embed::LocalEmbedder::new(EMBED_DIM)),
        );
        // connect on b should infer edges (semantic/entity) to a.
        let n = ctrl.connect(&b).await.unwrap();
        assert!(n >= 2, "expected symmetric semantic+entity edges, got {n}");

        // Retrieve from text near b should reach a through the graph.
        let q = RecallQuery::new("s1", "rust programming");
        let res = ctrl.retrieve(&q, vec![], 20, 3, 10).await.unwrap();
        let ids: Vec<&str> = res.items.iter().map(|i| i.fragment.id.as_str()).collect();
        assert!(ids.contains(&a.id.as_str()));
        assert!(ids.contains(&b.id.as_str()));
        assert!(!res.trace.stop_reason.is_empty());

        // Invalid retrieve params rejected.
        assert!(ctrl.retrieve(&q, vec![], 0, 3, 10).await.is_err());
    }

    #[tokio::test]
    async fn context_controller_rejects_missing_fragment() {
        let store = MemoryContextStore::new();
        let ghost = ContextFragment::new("s1", FragmentKind::LongTerm, "standalone");
        let ctrl = ContextController::with_defaults(
            store.clone(),
            Arc::new(embed::LocalEmbedder::new(EMBED_DIM)),
        );
        assert!(matches!(
            ctrl.connect(&ghost).await,
            Err(ContextError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn composite_merges_relations() {
        let local = MemoryContextStore::new();
        let cloud = MemoryContextStore::new();
        let composite = CompositeContextStore::new(local.clone(), cloud.clone());

        // Store fragments through the composite so they exist on both sides.
        composite
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::LongTerm,
                "alpha node",
            ))
            .await
            .unwrap();
        composite
            .memorize(ContextFragment::new(
                "s",
                FragmentKind::LongTerm,
                "beta node",
            ))
            .await
            .unwrap();
        let frags = composite.list_session("s", 10).await.unwrap();
        let a = frags.iter().find(|f| f.content.contains("alpha")).unwrap();
        let b = frags.iter().find(|f| f.content.contains("beta")).unwrap();

        composite
            .relate(Relation {
                session: "s".into(),
                from_id: a.id.clone(),
                to_id: b.id.clone(),
                kind: RelationKind::Semantic,
                score: 0.8,
                provenance: "local".into(),
                created_at: 5,
            })
            .await
            .unwrap();
        // The edge must be visible from both sides (writes to both).
        assert_eq!(
            local
                .get_relations("s", None, None, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            cloud
                .get_relations("s", None, None, None, 10)
                .await
                .unwrap()
                .len(),
            1
        );

        // get_relations via composite returns a single merged edge.
        let merged = composite
            .get_relations("s", None, None, None, 10)
            .await
            .unwrap();
        assert_eq!(merged.len(), 1);

        // expand across the composite reaches both nodes.
        let q = GraphRetrieveQuery {
            session: "s".into(),
            seeds: vec![a.id.clone()],
            views: vec![],
            budget: 10,
            max_hops: 3,
            top_k: 10,
        };
        let res = composite.expand(&q).await.unwrap();
        assert_eq!(res.trace.hits, 2);
    }

    // --- Abnormal paths (no DB) ---

    fn sample_relation() -> Relation {
        Relation {
            session: "s".into(),
            from_id: "a".into(),
            to_id: "b".into(),
            kind: RelationKind::Semantic,
            score: 0.5,
            provenance: "local".into(),
            created_at: 1,
        }
    }

    #[test]
    fn relation_validate_rejects_self_loop_and_bad_score() {
        let ok = sample_relation();
        assert!(ok.validate().is_ok());

        // Self-loop rejected (NotFound, never a valid edge).
        let mut self_loop = ok.clone();
        self_loop.to_id = "a".into();
        assert!(matches!(
            self_loop.validate(),
            Err(ContextError::NotFound(_))
        ));

        // Score out of [0,1] rejected (Storage).
        let mut bad = ok.clone();
        bad.score = 1.4;
        assert!(matches!(bad.validate(), Err(ContextError::Storage(_))));
        bad.score = -0.1;
        assert!(matches!(bad.validate(), Err(ContextError::Storage(_))));

        // Empty provenance rejected (Storage).
        let mut no_prov = ok.clone();
        no_prov.provenance = "   ".into();
        assert!(matches!(no_prov.validate(), Err(ContextError::Storage(_))));
    }

    #[test]
    fn graph_query_validate_rejects_bad_bounds() {
        let good = GraphRetrieveQuery {
            session: "s".into(),
            seeds: vec!["a".into()],
            views: vec![],
            budget: 10,
            max_hops: 3,
            top_k: 10,
        };
        assert!(good.validate().is_ok());

        let mut q = good.clone();
        q.seeds = vec![];
        assert!(q.validate().is_err(), "empty seeds rejected");

        q = good.clone();
        q.budget = 0;
        assert!(q.validate().is_err(), "budget=0 rejected");

        q = good.clone();
        q.max_hops = 0;
        assert!(q.validate().is_err(), "max_hops=0 rejected");

        q = good.clone();
        q.top_k = 0;
        assert!(q.validate().is_err(), "top_k=0 rejected");
    }

    #[tokio::test]
    async fn memory_store_expand_rejects_invalid_query() {
        let store = MemoryContextStore::new();
        store
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "x"))
            .await
            .unwrap();
        let bad = GraphRetrieveQuery {
            session: "s".into(),
            seeds: vec!["x".into()],
            views: vec![],
            budget: 0,
            max_hops: 3,
            top_k: 10,
        };
        assert!(store.expand(&bad).await.is_err());
    }

    #[tokio::test]
    async fn memory_store_relate_rejects_self_loop() {
        let store = MemoryContextStore::new();
        store
            .memorize(ContextFragment::new("s", FragmentKind::LongTerm, "x"))
            .await
            .unwrap();
        let id = store.list_session("s", 10).await.unwrap()[0].id.clone();
        let self_loop = Relation {
            session: "s".into(),
            from_id: id.clone(),
            to_id: id.clone(),
            kind: RelationKind::Semantic,
            score: 0.5,
            provenance: "local".into(),
            created_at: 1,
        };
        assert!(store.relate(self_loop).await.is_err());
    }
}
