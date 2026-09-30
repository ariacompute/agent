//! Cloud backend for the native SDK: a tiny REST client over the
//! **beta Agents** session memory endpoints.
//!
//! `POST /v1/agents/sessions/{id}/memory`          → memorize
//! `GET  /v1/agents/sessions/{id}/memory?text=…`   → recall
//! `GET  /v1/agents/sessions/{id}/memory`          → list (used by `compact`)
//! `GET  /v1/agents/sessions/{id}/memory/{key}`    → get_by_key

use agent_core::context::{
    ContextError, ContextFragment, ContextStore, FragmentKind, GraphRetrieveQuery,
    GraphRetrieveResult, RecallQuery, Relation, RelationKind, RetrieveTrace, ScoredFragment,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Serialize)]
struct PutMemoryBody<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<&'a str>,
    value: &'a str,
    kind: &'a str,
}

#[derive(Debug, Deserialize)]
struct MemoryItem {
    id: String,
    key: Option<String>,
    kind: String,
    content: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct ListResponse {
    data: Vec<MemoryItem>,
}

// --- Multi-relational memory plane: wire shapes for the cloud relation endpoints ---

#[derive(Debug, Serialize)]
struct CreateRelationBody<'a> {
    from_id: &'a str,
    to_id: &'a str,
    kind: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    provenance: Option<&'a str>,
}

#[derive(Debug, Deserialize)]
struct RelationItem {
    session: String,
    from_id: String,
    to_id: String,
    kind: String,
    score: f32,
    provenance: String,
    created_at: i64,
}

#[derive(Debug, Deserialize)]
struct RelationListResponse {
    data: Vec<RelationItem>,
}

#[derive(Debug, Deserialize)]
struct ScoredItem {
    id: String,
    key: Option<String>,
    kind: String,
    content: String,
    created_at: i64,
    score: f32,
}

#[derive(Debug, Deserialize)]
struct TraceResp {
    views: Vec<String>,
    budget: usize,
    stop_reason: String,
    hits: usize,
}

#[derive(Debug, Deserialize)]
struct GraphRetrieveResponse {
    items: Vec<ScoredItem>,
    trace: TraceResp,
}

#[derive(Debug, Deserialize)]
struct DeleteRelationResponse {
    deleted: usize,
}

fn kind_str(kind: FragmentKind) -> &'static str {
    match kind {
        FragmentKind::Message => "message",
        FragmentKind::ToolResult => "tool_result",
        FragmentKind::LongTerm => "long_term",
        FragmentKind::Note => "note",
    }
}

fn kind_of(s: &str) -> FragmentKind {
    match s {
        "tool_result" => FragmentKind::ToolResult,
        "note" => FragmentKind::Note,
        "message" => FragmentKind::Message,
        _ => FragmentKind::LongTerm,
    }
}

/// Cloud (agent-cloud + pgvector) backend used by the native SDK.
pub struct CloudContextStore {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
}

impl CloudContextStore {
    /// `base_url` is the cloud root (e.g. `http://localhost:3000`).
    pub fn new(base_url: &str, api_key: Option<&str>) -> Result<Arc<Self>, ContextError> {
        let base = base_url.trim().trim_end_matches('/');
        if base.is_empty() {
            return Err(ContextError::Storage("cloud base url is empty".into()));
        }
        Ok(Arc::new(Self {
            client: reqwest::Client::new(),
            base_url: base.to_string(),
            api_key: api_key
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty()),
        }))
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut req = self
            .client
            .request(method, format!("{}{}", self.base_url, path))
            .header("OpenAI-Beta", "agents=v1");
        if let Some(key) = &self.api_key {
            req = req.bearer_auth(key);
        }
        req
    }

    async fn items(&self, path: &str) -> Result<Vec<MemoryItem>, ContextError> {
        let res = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud request failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud returned {}",
                res.status()
            )));
        }
        let parsed: ListResponse = res
            .json()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud decode failed: {e}")))?;
        Ok(parsed.data)
    }

    async fn relation_items(&self, path: &str) -> Result<Vec<RelationItem>, ContextError> {
        let res = self
            .request(reqwest::Method::GET, path)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud request failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud returned {}",
                res.status()
            )));
        }
        let parsed: RelationListResponse = res
            .json()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud decode failed: {e}")))?;
        Ok(parsed.data)
    }
}

#[async_trait]
impl ContextStore for CloudContextStore {
    async fn memorize(&self, frag: ContextFragment) -> Result<(), ContextError> {
        let path = format!("/v1/agents/sessions/{}/memory", urlencoding(&frag.session));
        let body = PutMemoryBody {
            key: frag.key.as_deref(),
            value: &frag.content,
            kind: kind_str(frag.kind),
        };
        let res = self
            .request(reqwest::Method::POST, &path)
            .json(&body)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud write failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud write returned {}",
                res.status()
            )));
        }
        Ok(())
    }

    async fn recall(&self, query: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
        if query.text.trim().is_empty() {
            return Ok(Vec::new());
        }
        let path = format!(
            "/v1/agents/sessions/{}/memory?text={}&top_k={}",
            urlencoding(&query.session),
            urlencoding(&query.text),
            query.top_k
        );
        let items = self.items(&path).await?;
        let mut out: Vec<ContextFragment> = items
            .into_iter()
            .filter(|i| query.kind.map(|k| kind_of(&i.kind) == k).unwrap_or(true))
            .map(|i| ContextFragment {
                id: i.id,
                session: query.session.clone(),
                key: i.key,
                kind: kind_of(&i.kind),
                content: i.content,
                created_at: i.created_at,
                embedding: None,
            })
            .collect();
        out.truncate(query.top_k);
        Ok(out)
    }

    async fn compact(&self, session: &str) -> Result<ContextFragment, ContextError> {
        let path = format!(
            "/v1/agents/sessions/{}/memory?top_k=100",
            urlencoding(session)
        );
        let items = self.items(&path).await?;
        if items.is_empty() {
            return Err(ContextError::NotFound(session.to_string()));
        }
        let joined = items
            .iter()
            .map(|i| format!("[{}] {}", i.kind, i.content))
            .collect::<Vec<_>>()
            .join("\n---\n");
        let merged = ContextFragment::new(session, FragmentKind::Note, joined)
            .with_key(format!("__compact__{session}"));
        self.memorize(merged.clone()).await?;
        Ok(merged)
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<ContextFragment>, ContextError> {
        let path = format!(
            "/v1/agents/sessions/{}/memory/{}",
            urlencoding(session),
            urlencoding(key)
        );
        let res = self
            .request(reqwest::Method::GET, &path)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud read failed: {e}")))?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud read returned {}",
                res.status()
            )));
        }
        let item: MemoryItem = res
            .json()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud decode failed: {e}")))?;
        Ok(Some(ContextFragment {
            id: item.id,
            session: session.to_string(),
            key: item.key,
            kind: kind_of(&item.kind),
            content: item.content,
            created_at: item.created_at,
            embedding: None,
        }))
    }

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<ContextFragment>, ContextError> {
        let path = format!(
            "/v1/agents/sessions/{}/memory?top_k={}",
            urlencoding(session),
            top_k
        );
        let items = self.items(&path).await?;
        Ok(items
            .into_iter()
            .map(|i| ContextFragment {
                id: i.id,
                session: session.to_string(),
                key: i.key,
                kind: kind_of(&i.kind),
                content: i.content,
                created_at: i.created_at,
                embedding: None,
            })
            .collect())
    }

    async fn relate(&self, rel: Relation) -> Result<(), ContextError> {
        let path = format!(
            "/v1/agents/sessions/{}/memory/relations",
            urlencoding(&rel.session)
        );
        let body = CreateRelationBody {
            from_id: &rel.from_id,
            to_id: &rel.to_id,
            kind: rel.kind.as_str(),
            score: Some(rel.score),
            provenance: Some(&rel.provenance),
        };
        let res = self
            .request(reqwest::Method::POST, &path)
            .json(&body)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud relate failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud relate returned {}",
                res.status()
            )));
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
        let mut path = format!(
            "/v1/agents/sessions/{}/memory/relations?top_k={}",
            urlencoding(session),
            top_k
        );
        if let Some(f) = from {
            path.push_str(&format!("&from={}", urlencoding(f)));
        }
        if let Some(t) = to {
            path.push_str(&format!("&to={}", urlencoding(t)));
        }
        if let Some(k) = kind {
            path.push_str(&format!("&kind={}", k.as_str()));
        }
        let items = self.relation_items(&path).await?;
        items
            .into_iter()
            .map(|i| {
                let kind: RelationKind = i
                    .kind
                    .parse()
                    .map_err(|e: String| ContextError::Storage(format!("bad relation kind: {e}")))?;
                Ok(Relation {
                    session: i.session,
                    from_id: i.from_id,
                    to_id: i.to_id,
                    kind,
                    score: i.score,
                    provenance: i.provenance,
                    created_at: i.created_at,
                })
            })
            .collect()
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
        let mut path = format!(
            "/v1/agents/sessions/{}/memory/relations",
            urlencoding(session)
        );
        let mut q = Vec::new();
        if let Some(f) = from {
            q.push(format!("from={}", urlencoding(f)));
        }
        if let Some(t) = to {
            q.push(format!("to={}", urlencoding(t)));
        }
        if let Some(k) = kind {
            q.push(format!("kind={}", k.as_str()));
        }
        if !q.is_empty() {
            path.push('?');
            path.push_str(&q.join("&"));
        }
        let res = self
            .request(reqwest::Method::DELETE, &path)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud delete failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud delete returned {}",
                res.status()
            )));
        }
        let parsed: DeleteRelationResponse = res
            .json()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud decode failed: {e}")))?;
        Ok(parsed.deleted)
    }

    async fn expand(&self, query: &GraphRetrieveQuery) -> Result<GraphRetrieveResult, ContextError> {
        let seeds = query
            .seeds
            .iter()
            .map(|s| urlencoding(s))
            .collect::<Vec<_>>()
            .join(",");
        let views = query
            .views
            .iter()
            .map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let path = format!(
            "/v1/agents/sessions/{}/memory/relations/graph?seeds={}&views={}&budget={}&max_hops={}&top_k={}",
            urlencoding(&query.session),
            seeds,
            views,
            query.budget,
            query.max_hops,
            query.top_k
        );
        let res = self
            .request(reqwest::Method::GET, &path)
            .send()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud expand failed: {e}")))?;
        if !res.status().is_success() {
            return Err(ContextError::Storage(format!(
                "cloud expand returned {}",
                res.status()
            )));
        }
        let parsed: GraphRetrieveResponse = res
            .json()
            .await
            .map_err(|e| ContextError::Storage(format!("cloud decode failed: {e}")))?;
        let items = parsed
            .items
            .into_iter()
            .map(|i| ScoredFragment {
                fragment: ContextFragment {
                    id: i.id,
                    session: query.session.clone(),
                    key: i.key,
                    kind: kind_of(&i.kind),
                    content: i.content,
                    created_at: i.created_at,
                    embedding: None,
                },
                score: i.score,
            })
            .collect();
        let views = parsed
            .trace
            .views
            .iter()
            .map(|v| {
                v.parse::<RelationKind>()
                    .map_err(|e: String| ContextError::Storage(format!("bad relation kind: {e}")))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(GraphRetrieveResult {
            items,
            trace: RetrieveTrace {
                views,
                budget: parsed.trace.budget,
                stop_reason: parsed.trace.stop_reason,
                hits: parsed.trace.hits,
            },
        })
    }
}

/// Minimal percent-encoding for path/query segments (no external dependency).
fn urlencoding(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urlencoding_escapes_segments() {
        assert_eq!(urlencoding("sess_1"), "sess_1");
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("a/b?c"), "a%2Fb%3Fc");
    }

    #[test]
    fn empty_base_url_is_rejected() {
        assert!(CloudContextStore::new("  ", None).is_err());
    }

    #[test]
    fn base_url_is_normalized() {
        let store = CloudContextStore::new("http://aria.test/", Some("key")).unwrap();
        assert_eq!(store.base_url, "http://aria.test");
        assert_eq!(store.api_key.as_deref(), Some("key"));
    }

    #[test]
    fn kind_mapping_roundtrips() {
        for kind in [
            FragmentKind::Message,
            FragmentKind::ToolResult,
            FragmentKind::LongTerm,
            FragmentKind::Note,
        ] {
            assert_eq!(kind_of(kind_str(kind)), kind);
        }
    }
}
