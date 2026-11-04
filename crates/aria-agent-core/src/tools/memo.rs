//! The long-term memory tool family exposed to the agentic loop, and the
//! [`MemoToolHandler`] that routes `memo_*` tool calls to the agent's
//! [`ContextStore`].
//!
//! The four tools let the model **actively** manage its own memory during a run:
//! `memo_store` / `memo_get` (write / read a keyed long-term memory),
//! `memo_search` (semantic recall) and `memo_related` (inspect the
//! multi-relational memory graph for the session).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::context::{ContextError, ContextFragment, ContextStore, FragmentKind, RecallQuery};
use crate::{Tool, ToolCall, ToolHandler, ToolResult};

/// Build the long-term memory tools the model can call. Mirrors the frozen tool
/// contract (`Tool { name, description, parameters }`). These are appended to (not
/// replacing) the shell tool.
pub fn memo_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "memo_store".into(),
            description:
                "Persist a long-term memory under a key so it survives across turns (e.g. user preferences)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Stable key for the memory" },
                    "value": { "type": "string", "description": "The memory content" }
                },
                "required": ["key", "value"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memo_get".into(),
            description: "Read a long-term memory back by its key.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Key previously stored with memo_store" }
                },
                "required": ["key"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memo_search".into(),
            description:
                "Semantic search over the session's memory (vector recall); returns the most relevant fragments."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query" },
                    "top_k": { "type": "integer", "description": "Max fragments to return (default 8)" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "memo_related".into(),
            description:
                "Inspect the multi-relational memory graph for the session: list the edges \
                 (from/to/kind/score). Optionally restrict to edges originating from a given \
                 fragment `id`."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Optional seed fragment id to filter edges" }
                },
                "required": [],
                "additionalProperties": false
            }),
        },
    ]
}

/// Routes `memo_*` tool calls to the agent's [`ContextStore`].
pub struct MemoToolHandler {
    store: Arc<dyn ContextStore>,
}

impl MemoToolHandler {
    pub fn new(store: Arc<dyn ContextStore>) -> Self {
        Self { store }
    }

    async fn exec(&self, call: &ToolCall, session: &str) -> Result<String, ContextError> {
        let a: &Value = &call.arguments;
        let arg_str = |k: &str| a.get(k).and_then(|v| v.as_str()).map(|s| s.to_string());
        let arg_u64 = |k: &str| a.get(k).and_then(|v| v.as_u64()).map(|n| n as usize);
        match call.name.as_str() {
            "memo_store" => {
                let key = arg_str("key")
                    .ok_or_else(|| ContextError::Storage("memo_store requires `key`".into()))?;
                let value = arg_str("value")
                    .ok_or_else(|| ContextError::Storage("memo_store requires `value`".into()))?;
                self.store
                    .memorize(
                        ContextFragment::new(session, FragmentKind::LongTerm, value)
                            .with_key(key.clone()),
                    )
                    .await?;
                Ok(format!("stored memory under key `{key}`"))
            }
            "memo_get" => {
                let key = arg_str("key")
                    .ok_or_else(|| ContextError::Storage("memo_get requires `key`".into()))?;
                match self.store.get_by_key(session, &key).await? {
                    Some(f) => Ok(f.content),
                    None => Ok("(no memory found for that key)".into()),
                }
            }
            "memo_search" => {
                let query = arg_str("query")
                    .ok_or_else(|| ContextError::Storage("memo_search requires `query`".into()))?;
                let top_k = arg_u64("top_k").unwrap_or(8);
                let frags = self.store.recall(&RecallQuery::new(session, query)).await?;
                if frags.is_empty() {
                    return Ok("(no matching memories)".into());
                }
                let out: Vec<String> = frags
                    .into_iter()
                    .take(top_k)
                    .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
                    .collect();
                Ok(out.join("\n---\n"))
            }
            "memo_related" => {
                let from = arg_str("id");
                let rels = self
                    .store
                    .get_relations(session, from.as_deref(), None, None, 50)
                    .await?;
                if rels.is_empty() {
                    return Ok("(no memory-graph edges)".into());
                }
                // `Vec<Relation>` always serializes, so the fallback never runs;
                // `unwrap_or_default` keeps that branch free of a dead closure.
                Ok(serde_json::to_string(&rels).unwrap_or_default())
            }
            other => Err(ContextError::Storage(format!("unknown memo tool: {other}"))),
        }
    }
}

#[async_trait]
impl ToolHandler for MemoToolHandler {
    fn handles(&self, name: &str) -> bool {
        name.starts_with("memo_")
    }

    async fn run(&self, call: &ToolCall, ctx: &Arc<dyn ContextStore>, session: &str) -> ToolResult {
        let (content, is_error) = match self.exec(call, session).await {
            Ok(s) => (s, false),
            Err(e) => (format!("ERROR: {e}"), true),
        };
        // Persist the tool result to memo so later turns can recall it.
        let _ = ctx
            .memorize(ContextFragment::new(
                session,
                FragmentKind::ToolResult,
                content.clone(),
            ))
            .await;
        ToolResult {
            call_id: call.id.clone(),
            content,
            is_error,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::{
        GraphRetrieveQuery, GraphRetrieveResult, MemoryContextStore, Relation, RelationKind,
    };

    fn call(name: &str, arguments: Value) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments,
        }
    }

    fn memory() -> Arc<dyn ContextStore> {
        MemoryContextStore::new()
    }

    /// A store whose every operation fails, to exercise the error branches.
    struct FailingStore;

    #[async_trait]
    impl ContextStore for FailingStore {
        async fn memorize(&self, _frag: ContextFragment) -> Result<(), ContextError> {
            Err(ContextError::Storage("memorize failed".into()))
        }
        async fn recall(&self, _q: &RecallQuery) -> Result<Vec<ContextFragment>, ContextError> {
            Err(ContextError::Storage("recall failed".into()))
        }
        async fn compact(&self, s: &str) -> Result<ContextFragment, ContextError> {
            Err(ContextError::NotFound(s.into()))
        }
        async fn get_by_key(
            &self,
            _s: &str,
            _k: &str,
        ) -> Result<Option<ContextFragment>, ContextError> {
            Err(ContextError::Storage("get_by_key failed".into()))
        }
        async fn list_session(
            &self,
            _s: &str,
            _k: usize,
        ) -> Result<Vec<ContextFragment>, ContextError> {
            Err(ContextError::Storage("list failed".into()))
        }
        async fn relate(&self, _r: Relation) -> Result<(), ContextError> {
            Err(ContextError::Storage("relate failed".into()))
        }
        async fn get_relations(
            &self,
            _s: &str,
            _f: Option<&str>,
            _t: Option<&str>,
            _k: Option<RelationKind>,
            _top_k: usize,
        ) -> Result<Vec<Relation>, ContextError> {
            Err(ContextError::Storage("relations failed".into()))
        }
        async fn delete_relations(
            &self,
            _s: &str,
            _f: Option<&str>,
            _t: Option<&str>,
            _k: Option<RelationKind>,
        ) -> Result<usize, ContextError> {
            Err(ContextError::Storage("delete failed".into()))
        }
        async fn expand(
            &self,
            _q: &GraphRetrieveQuery,
        ) -> Result<GraphRetrieveResult, ContextError> {
            Err(ContextError::Storage("expand failed".into()))
        }
    }

    fn edge(session: &str, from: &str, to: &str) -> Relation {
        Relation {
            session: session.into(),
            from_id: from.into(),
            to_id: to.into(),
            kind: RelationKind::Temporal,
            score: 0.9,
            provenance: "local:temporal".into(),
            created_at: 1,
        }
    }

    #[test]
    fn tool_family_is_the_frozen_contract() {
        let tools = memo_tools();
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["memo_store", "memo_get", "memo_search", "memo_related"]
        );
        for tool in &tools {
            assert!(tool.parameters.get("type").is_some(), "{}", tool.name);
        }
        let handler = MemoToolHandler::new(memory());
        assert!(handler.handles("memo_store"));
        assert!(handler.handles("memo_related"));
        assert!(!handler.handles("shell"));
        assert!(!handler.handles(""));
    }

    #[tokio::test]
    async fn store_requires_a_key_and_a_value() {
        let handler = MemoToolHandler::new(memory());
        let store = memory();

        let missing_key = handler
            .run(&call("memo_store", json!({ "value": "v" })), &store, "s")
            .await;
        assert!(missing_key.is_error);
        assert!(missing_key.content.contains("requires `key`"));

        let missing_value = handler
            .run(&call("memo_store", json!({ "key": "k" })), &store, "s")
            .await;
        assert!(missing_value.is_error);
        assert!(missing_value.content.contains("requires `value`"));
    }

    #[tokio::test]
    async fn store_then_get_roundtrips_through_the_handler() {
        let store = memory();
        let handler = MemoToolHandler::new(store.clone());

        let stored = handler
            .run(
                &call(
                    "memo_store",
                    json!({ "key": "pref", "value": "likes rust" }),
                ),
                &store,
                "s1",
            )
            .await;
        assert!(!stored.is_error, "{}", stored.content);
        assert_eq!(stored.content, "stored memory under key `pref`");

        let fetched = handler
            .run(&call("memo_get", json!({ "key": "pref" })), &store, "s1")
            .await;
        assert!(!fetched.is_error);
        assert_eq!(fetched.content, "likes rust");

        let absent = handler
            .run(&call("memo_get", json!({ "key": "nope" })), &store, "s1")
            .await;
        assert!(!absent.is_error);
        assert_eq!(absent.content, "(no memory found for that key)");

        let no_key = handler
            .run(&call("memo_get", json!({})), &store, "s1")
            .await;
        assert!(no_key.is_error);
        assert!(no_key.content.contains("memo_get requires `key`"));
    }

    #[tokio::test]
    async fn search_requires_a_query_and_honours_top_k() {
        let store = memory();
        let handler = MemoToolHandler::new(store.clone());
        for (key, value) in [("a", "alpha note"), ("b", "beta note"), ("c", "gamma note")] {
            store
                .memorize(
                    ContextFragment::new("s2", FragmentKind::LongTerm, value)
                        .with_key(key.to_string()),
                )
                .await
                .unwrap();
        }

        let missing = handler
            .run(&call("memo_search", json!({})), &store, "s2")
            .await;
        assert!(missing.is_error);
        assert!(missing.content.contains("memo_search requires `query`"));

        let all = handler
            .run(
                &call("memo_search", json!({ "query": "note" })),
                &store,
                "s2",
            )
            .await;
        assert!(!all.is_error);
        assert_eq!(all.content.matches("---").count(), 2);

        let capped = handler
            .run(
                &call("memo_search", json!({ "query": "note", "top_k": 1 })),
                &store,
                "s2",
            )
            .await;
        assert!(!capped.is_error);
        assert!(!capped.content.contains("---"), "{}", capped.content);

        let none = handler
            .run(
                &call("memo_search", json!({ "query": "zebra" })),
                &store,
                "s3",
            )
            .await;
        assert!(!none.is_error);
        assert_eq!(none.content, "(no matching memories)");
    }

    #[tokio::test]
    async fn related_lists_edges_and_filters_by_seed() {
        let store = memory();
        let handler = MemoToolHandler::new(store.clone());
        let a = ContextFragment::new("s4", FragmentKind::Message, "first");
        let b = ContextFragment::new("s4", FragmentKind::Message, "second");
        let (id_a, id_b) = (a.id.clone(), b.id.clone());
        store.memorize(a).await.unwrap();
        store.memorize(b).await.unwrap();
        store.relate(edge("s4", &id_a, &id_b)).await.unwrap();

        let all = handler
            .run(&call("memo_related", json!({})), &store, "s4")
            .await;
        assert!(!all.is_error);
        assert!(all.content.contains(&id_a));

        let filtered = handler
            .run(&call("memo_related", json!({ "id": &id_a })), &store, "s4")
            .await;
        assert!(!filtered.is_error);
        assert!(filtered.content.contains(&id_b));

        let empty = handler
            .run(&call("memo_related", json!({})), &store, "s5")
            .await;
        assert!(!empty.is_error);
        assert_eq!(empty.content, "(no memory-graph edges)");
    }

    #[tokio::test]
    async fn unknown_tools_are_reported_as_errors() {
        let store = memory();
        let handler = MemoToolHandler::new(store.clone());
        let res = handler
            .run(&call("memo_bogus", json!({})), &store, "s")
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("unknown memo tool: memo_bogus"));
    }

    #[tokio::test]
    async fn store_failures_surface_as_error_results() {
        let store: Arc<dyn ContextStore> = Arc::new(FailingStore);
        let handler = MemoToolHandler::new(store.clone());

        for (name, args) in [
            ("memo_store", json!({ "key": "k", "value": "v" })),
            ("memo_get", json!({ "key": "k" })),
            ("memo_search", json!({ "query": "q" })),
            ("memo_related", json!({})),
        ] {
            let res = handler.run(&call(name, args), &store, "s").await;
            assert!(res.is_error, "{name} should fail");
            assert!(res.content.starts_with("ERROR: "), "{}", res.content);
        }
    }

    #[tokio::test]
    async fn tool_results_are_persisted_to_the_context() {
        let store = memory();
        let handler = MemoToolHandler::new(store.clone());
        let res = handler
            .run(
                &call("memo_store", json!({ "key": "k", "value": "v" })),
                &store,
                "s6",
            )
            .await;
        let persisted = store.list_session("s6", 10).await.unwrap();
        assert!(
            persisted
                .iter()
                .any(|f| f.kind == FragmentKind::ToolResult && f.content == res.content),
            "tool result must be persisted"
        );
    }

    #[tokio::test]
    async fn failing_store_fails_every_operation() {
        // Keeps the test double honest: nothing silently succeeds.
        let store: Arc<dyn ContextStore> = Arc::new(FailingStore);
        assert!(store
            .memorize(ContextFragment::new("s", FragmentKind::Note, "x"))
            .await
            .is_err());
        assert!(store.compact("s").await.is_err());
        assert!(store.list_session("s", 1).await.is_err());
        assert!(store.relate(edge("s", "a", "b")).await.is_err());
        assert!(store
            .delete_relations("s", Some("a"), None, None)
            .await
            .is_err());
        assert!(store
            .expand(&GraphRetrieveQuery {
                session: "s".into(),
                seeds: vec!["a".into()],
                views: vec![],
                budget: 1,
                max_hops: 1,
                top_k: 1,
            })
            .await
            .is_err());
    }
}
