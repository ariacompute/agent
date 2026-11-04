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
                let kind: RelationKind = i.kind.parse().map_err(|e: String| {
                    ContextError::Storage(format!("bad relation kind: {e}"))
                })?;
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

    async fn expand(
        &self,
        query: &GraphRetrieveQuery,
    ) -> Result<GraphRetrieveResult, ContextError> {
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
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn urlencoding_escapes_segments() {
        assert_eq!(urlencoding("sess_1"), "sess_1");
        assert_eq!(urlencoding("a b"), "a%20b");
        assert_eq!(urlencoding("a/b?c"), "a%2Fb%3Fc");
        assert_eq!(urlencoding("~-._"), "~-._");
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
    fn blank_api_key_is_dropped() {
        let store = CloudContextStore::new("http://aria.test", Some("   ")).unwrap();
        assert!(store.api_key.is_none());
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
        // Unknown wire values degrade to long-term rather than failing the read.
        assert_eq!(kind_of("mystery"), FragmentKind::LongTerm);
    }

    // --------------------------------------------------------------- fixtures

    fn item(id: &str, key: Option<&str>, kind: &str, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id,
            "key": key,
            "kind": kind,
            "content": content,
            "created_at": 42,
        })
    }

    fn relation(from: &str, to: &str, kind: &str) -> serde_json::Value {
        serde_json::json!({
            "session": "s1",
            "from_id": from,
            "to_id": to,
            "kind": kind,
            "score": 0.75,
            "provenance": "local:semantic",
            "created_at": 9,
        })
    }

    fn list_body(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "data": items })
    }

    fn rel_list_body(items: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({ "data": items })
    }

    fn store(server: &MockServer) -> Arc<CloudContextStore> {
        CloudContextStore::new(&server.uri(), Some("secret")).unwrap()
    }

    fn unreachable() -> Arc<CloudContextStore> {
        CloudContextStore::new("http://127.0.0.1:0", None).unwrap()
    }

    // --------------------------------------------------------------- memorize

    #[tokio::test]
    async fn memorize_posts_the_fragment_with_auth() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents/sessions/s%201/memory"))
            .respond_with(ResponseTemplate::new(201))
            .expect(1)
            .mount(&server)
            .await;

        store(&server)
            .memorize(ContextFragment::new("s 1", FragmentKind::ToolResult, "hi").with_key("k1"))
            .await
            .unwrap();

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs.len(), 1);
        let req = &reqs[0];
        assert_eq!(req.url.path(), "/v1/agents/sessions/s%201/memory");
        assert_eq!(req.url.query(), None);
        assert_eq!(req.headers.get("openai-beta").unwrap(), "agents=v1");
        assert_eq!(req.headers.get("authorization").unwrap(), "Bearer secret");
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["key"], "k1");
        assert_eq!(body["value"], "hi");
        assert_eq!(body["kind"], "tool_result");
    }

    #[tokio::test]
    async fn memorize_omits_a_missing_key_and_the_auth_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(1)
            .mount(&server)
            .await;

        CloudContextStore::new(&server.uri(), None)
            .unwrap()
            .memorize(ContextFragment::new("s1", FragmentKind::Note, "body"))
            .await
            .unwrap();

        let req = &server.received_requests().await.unwrap()[0];
        assert!(req.headers.get("authorization").is_none());
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert!(body.get("key").is_none());
        assert_eq!(body["kind"], "note");
    }

    #[tokio::test]
    async fn memorize_reports_error_statuses_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let err = store(&server)
            .memorize(ContextFragment::new("s1", FragmentKind::Note, "x"))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cloud write returned 500"),
            "{err}"
        );

        let err = unreachable()
            .memorize(ContextFragment::new("s1", FragmentKind::Note, "x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud write failed"), "{err}");
    }

    // ----------------------------------------------------------------- recall

    #[tokio::test]
    async fn recall_short_circuits_on_blank_text() {
        let server = MockServer::start().await;
        let out = store(&server)
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "   ".into(),
                top_k: 3,
                kind: None,
            })
            .await
            .unwrap();
        assert!(out.is_empty());
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn recall_filters_by_kind_and_truncates() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s1/memory"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list_body(vec![
                item("a", Some("ka"), "message", "hello"),
                item("b", None, "note", "noted"),
                item("c", None, "tool_result", "out"),
            ])))
            .mount(&server)
            .await;

        let filtered = store(&server)
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "a b".into(),
                top_k: 2,
                kind: Some(FragmentKind::Note),
            })
            .await
            .unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].id, "b");
        assert_eq!(filtered[0].kind, FragmentKind::Note);
        assert_eq!(filtered[0].session, "s1");

        let query = server.received_requests().await.unwrap()[0]
            .url
            .query()
            .unwrap()
            .to_string();
        assert_eq!(query, "text=a%20b&top_k=2");
    }

    #[tokio::test]
    async fn recall_maps_every_wire_kind_and_spreads_the_cap() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list_body(vec![
                item("a", None, "message", "1"),
                item("b", None, "tool_result", "2"),
                item("c", None, "note", "3"),
                item("d", None, "long_term", "4"),
            ])))
            .mount(&server)
            .await;

        let out = store(&server)
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "q".into(),
                top_k: 3,
                kind: None,
            })
            .await
            .unwrap();
        assert_eq!(out.len(), 3);
        assert_eq!(
            out.iter().map(|f| f.kind).collect::<Vec<_>>(),
            vec![
                FragmentKind::Message,
                FragmentKind::ToolResult,
                FragmentKind::Note
            ]
        );
    }

    #[tokio::test]
    async fn recall_reports_status_and_decode_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(503))
            .mount(&server)
            .await;
        let err = store(&server)
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "q".into(),
                top_k: 1,
                kind: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud returned 503"), "{err}");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
            .mount(&server)
            .await;
        let err = store(&server)
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "q".into(),
                top_k: 1,
                kind: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud decode failed"), "{err}");

        let err = unreachable()
            .recall(&RecallQuery {
                session: "s1".into(),
                text: "q".into(),
                top_k: 1,
                kind: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud request failed"), "{err}");
    }

    // ---------------------------------------------------------------- compact

    #[tokio::test]
    async fn compact_joins_and_persists_a_note() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s1/memory"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list_body(vec![
                item("a", None, "message", "hi"),
                item("b", None, "note", "yo"),
            ])))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        let merged = store(&server).compact("s1").await.unwrap();
        assert_eq!(merged.kind, FragmentKind::Note);
        assert_eq!(merged.key.as_deref(), Some("__compact__s1"));
        assert_eq!(merged.content, "[message] hi\n---\n[note] yo");

        let reqs = server.received_requests().await.unwrap();
        assert_eq!(reqs[0].url.query(), Some("top_k=100"));
        let body: serde_json::Value = serde_json::from_slice(&reqs[1].body).unwrap();
        assert_eq!(body["key"], "__compact__s1");
        assert_eq!(body["kind"], "note");
        assert_eq!(body["value"], merged.content);
    }

    #[tokio::test]
    async fn compact_reports_an_empty_session_as_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list_body(vec![])))
            .mount(&server)
            .await;

        let err = store(&server).compact("s1").await.unwrap_err();
        assert!(
            matches!(err, ContextError::NotFound(ref s) if s == "s1"),
            "{err}"
        );
    }

    // ------------------------------------------------------------- get_by_key

    #[tokio::test]
    async fn get_by_key_maps_404_to_none_and_decodes_hits() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s1/memory/missing"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(store(&server)
            .get_by_key("s1", "missing")
            .await
            .unwrap()
            .is_none());

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s%201/memory/k%20v"))
            .respond_with(ResponseTemplate::new(200).set_body_json(item(
                "i1",
                Some("k v"),
                "note",
                "body",
            )))
            .mount(&server)
            .await;
        let frag = store(&server)
            .get_by_key("s 1", "k v")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(frag.id, "i1");
        assert_eq!(frag.session, "s 1");
        assert_eq!(frag.key.as_deref(), Some("k v"));
        assert_eq!(frag.kind, FragmentKind::Note);
        assert_eq!(frag.content, "body");
        assert_eq!(frag.created_at, 42);
    }

    #[tokio::test]
    async fn get_by_key_reports_status_decode_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = store(&server).get_by_key("s1", "k").await.unwrap_err();
        assert!(err.to_string().contains("cloud read returned 500"), "{err}");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html>"))
            .mount(&server)
            .await;
        let err = store(&server).get_by_key("s1", "k").await.unwrap_err();
        assert!(err.to_string().contains("cloud decode failed"), "{err}");

        let err = unreachable().get_by_key("s1", "k").await.unwrap_err();
        assert!(err.to_string().contains("cloud read failed"), "{err}");
    }

    // ----------------------------------------------------------- list_session

    #[tokio::test]
    async fn list_session_requests_the_cap_and_maps_items() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s1/memory"))
            .respond_with(ResponseTemplate::new(200).set_body_json(list_body(vec![
                item("a", Some("ka"), "tool_result", "one"),
                item("b", None, "mystery", "two"),
            ])))
            .mount(&server)
            .await;

        let items = store(&server).list_session("s1", 3).await.unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].key.as_deref(), Some("ka"));
        assert_eq!(items[0].kind, FragmentKind::ToolResult);
        assert_eq!(items[1].kind, FragmentKind::LongTerm);
        assert_eq!(
            server.received_requests().await.unwrap()[0].url.query(),
            Some("top_k=3")
        );
    }

    #[tokio::test]
    async fn list_session_propagates_store_errors() {
        let err = unreachable().list_session("s1", 1).await.unwrap_err();
        assert!(err.to_string().contains("cloud request failed"), "{err}");
    }

    // ---------------------------------------------------------- relate family

    fn sample_relation() -> Relation {
        Relation {
            session: "s 1".into(),
            from_id: "a".into(),
            to_id: "b".into(),
            kind: RelationKind::Causal,
            score: 0.5,
            provenance: "llm:causal".into(),
            created_at: 1,
        }
    }

    #[tokio::test]
    async fn relate_posts_the_edge() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/agents/sessions/s%201/memory/relations"))
            .respond_with(ResponseTemplate::new(201))
            .mount(&server)
            .await;

        store(&server).relate(sample_relation()).await.unwrap();

        let req = &server.received_requests().await.unwrap()[0];
        let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(body["from_id"], "a");
        assert_eq!(body["to_id"], "b");
        assert_eq!(body["kind"], "causal");
        assert_eq!(body["score"], 0.5);
        assert_eq!(body["provenance"], "llm:causal");
    }

    #[tokio::test]
    async fn relate_reports_status_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(409))
            .mount(&server)
            .await;
        let err = store(&server).relate(sample_relation()).await.unwrap_err();
        assert!(
            err.to_string().contains("cloud relate returned 409"),
            "{err}"
        );

        let err = unreachable().relate(sample_relation()).await.unwrap_err();
        assert!(err.to_string().contains("cloud relate failed"), "{err}");
    }

    #[tokio::test]
    async fn get_relations_builds_filters_and_maps_edges() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s1/memory/relations"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(rel_list_body(vec![relation("a", "b", "semantic")])),
            )
            .mount(&server)
            .await;

        let edges = store(&server)
            .get_relations("s1", Some("a b"), Some("b"), Some(RelationKind::Causal), 5)
            .await
            .unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, RelationKind::Semantic);
        assert_eq!(edges[0].score, 0.75);
        assert_eq!(edges[0].provenance, "local:semantic");
        assert_eq!(
            server.received_requests().await.unwrap()[0].url.query(),
            Some("top_k=5&from=a%20b&to=b&kind=causal")
        );
    }

    #[tokio::test]
    async fn get_relations_without_filters_has_a_bare_top_k_query() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(rel_list_body(vec![])))
            .mount(&server)
            .await;
        let edges = store(&server)
            .get_relations("s1", None, None, None, 2)
            .await
            .unwrap();
        assert!(edges.is_empty());
        assert_eq!(
            server.received_requests().await.unwrap()[0].url.query(),
            Some("top_k=2")
        );
    }

    #[tokio::test]
    async fn get_relations_rejects_unknown_kinds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(rel_list_body(vec![relation("a", "b", "weird")])),
            )
            .mount(&server)
            .await;
        let err = store(&server)
            .get_relations("s1", None, None, None, 2)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("bad relation kind: unknown relation_kind: weird"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn get_relations_reports_status_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502))
            .mount(&server)
            .await;
        let err = store(&server)
            .get_relations("s1", None, None, None, 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud returned 502"), "{err}");

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("[]"))
            .mount(&server)
            .await;
        let err = store(&server)
            .get_relations("s1", None, None, None, 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud decode failed"), "{err}");

        let err = unreachable()
            .get_relations("s1", None, None, None, 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud request failed"), "{err}");
    }

    // ------------------------------------------------------- delete_relations

    #[tokio::test]
    async fn delete_relations_requires_a_filter() {
        let err = unreachable()
            .delete_relations("s1", None, None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("delete_relations needs at least one filter"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn delete_relations_sends_the_filters_and_returns_the_count() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .and(path("/v1/agents/sessions/s1/memory/relations"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"deleted": 4})),
            )
            .mount(&server)
            .await;

        let deleted = store(&server)
            .delete_relations("s1", Some("a"), Some("b c"), Some(RelationKind::Entity))
            .await
            .unwrap();
        assert_eq!(deleted, 4);
        assert_eq!(
            server.received_requests().await.unwrap()[0].url.query(),
            Some("from=a&to=b%20c&kind=entity")
        );
    }

    #[tokio::test]
    async fn delete_relations_reports_status_decode_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = store(&server)
            .delete_relations("s1", Some("a"), None, None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("cloud delete returned 500"),
            "{err}"
        );

        let server = MockServer::start().await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(200).set_body_string("nope"))
            .mount(&server)
            .await;
        let err = store(&server)
            .delete_relations("s1", None, Some("b"), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud decode failed"), "{err}");

        let err = unreachable()
            .delete_relations("s1", Some("a"), None, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cloud delete failed"), "{err}");
    }

    // ----------------------------------------------------------------- expand

    fn graph_query() -> GraphRetrieveQuery {
        GraphRetrieveQuery {
            session: "s 1".into(),
            seeds: vec!["a b".into(), "c".into()],
            views: vec![RelationKind::Semantic, RelationKind::Causal],
            budget: 12,
            max_hops: 2,
            top_k: 3,
        }
    }

    #[tokio::test]
    async fn expand_encodes_the_query_and_maps_the_trace() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/agents/sessions/s%201/memory/relations/graph"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [{
                    "id": "i1",
                    "key": "k",
                    "kind": "note",
                    "content": "c",
                    "created_at": 5,
                    "score": 0.25,
                }],
                "trace": {
                    "views": ["semantic", "entity"],
                    "budget": 12,
                    "stop_reason": "budget",
                    "hits": 7,
                },
            })))
            .mount(&server)
            .await;

        let result = store(&server).expand(&graph_query()).await.unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].fragment.id, "i1");
        assert_eq!(result.items[0].fragment.session, "s 1");
        assert_eq!(result.items[0].score, 0.25);
        assert_eq!(
            result.trace.views,
            vec![RelationKind::Semantic, RelationKind::Entity]
        );
        assert_eq!(result.trace.budget, 12);
        assert_eq!(result.trace.stop_reason, "budget");
        assert_eq!(result.trace.hits, 7);
        assert_eq!(
            server.received_requests().await.unwrap()[0].url.query(),
            Some("seeds=a%20b,c&views=semantic,causal&budget=12&max_hops=2&top_k=3")
        );
    }

    #[tokio::test]
    async fn expand_rejects_unknown_trace_views() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "items": [],
                "trace": {"views": ["nope"], "budget": 1, "stop_reason": "done", "hits": 0},
            })))
            .mount(&server)
            .await;
        let err = store(&server).expand(&graph_query()).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("bad relation kind: unknown relation_kind: nope"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn expand_reports_status_decode_and_transport_failures() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let err = store(&server).expand(&graph_query()).await.unwrap_err();
        assert!(
            err.to_string().contains("cloud expand returned 500"),
            "{err}"
        );

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("{"))
            .mount(&server)
            .await;
        let err = store(&server).expand(&graph_query()).await.unwrap_err();
        assert!(err.to_string().contains("cloud decode failed"), "{err}");

        let err = unreachable().expand(&graph_query()).await.unwrap_err();
        assert!(err.to_string().contains("cloud expand failed"), "{err}");
    }
}
