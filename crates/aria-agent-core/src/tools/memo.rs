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
                Ok(serde_json::to_string(&rels).unwrap_or_else(|_| "[]".into()))
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
