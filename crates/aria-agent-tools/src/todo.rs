//! Todo tool family (`todo_*`).
//!
//! The list is a keyed `Note` fragment holding a JSON array, so it survives
//! restarts and can be projected into the session view without a new table.
//! Statuses are validated: an unknown status is rejected instead of silently
//! rendered as "pending".

use crate::{family_handler, Family};
use agent_core::context::{ContextFragment, ContextStore, FragmentKind};
use agent_core::seams::SeamError;
use agent_core::{Tool, ToolCall};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::sync::Arc;

/// Allowed task states.
pub const STATUSES: &[&str] = &["pending", "in_progress", "completed", "cancelled"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub content: String,
    #[serde(default = "default_status")]
    pub status: String,
}

fn default_status() -> String {
    "pending".into()
}

/// Key under which the todo list is stored for a session.
pub fn todo_key(session: &str) -> String {
    format!("todos:{session}")
}

pub struct TodoToolHandler {
    store: Arc<dyn ContextStore>,
}

impl TodoToolHandler {
    pub fn new(store: Arc<dyn ContextStore>) -> Self {
        Self { store }
    }

    async fn read(&self, session: &str) -> Result<Vec<Todo>, SeamError> {
        match self
            .store
            .get_by_key(session, &todo_key(session))
            .await
            .map_err(|e| SeamError::Io(e.to_string()))?
        {
            Some(f) => serde_json::from_str(&f.content)
                .map_err(|e| SeamError::Invalid(format!("stored todos are corrupt: {e}"))),
            None => Ok(Vec::new()),
        }
    }
}

pub fn todo_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "todo_write".into(),
            description: "Replace the session todo list.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "content": { "type": "string" },
                                "status": { "type": "string", "enum": STATUSES }
                            },
                            "required": ["id", "content"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["todos"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "todo_read".into(),
            description: "Read the session todo list.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for TodoToolHandler {
    fn prefix(&self) -> &'static str {
        "todo_"
    }

    async fn dispatch(&self, call: &ToolCall, session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "todo_write" => {
                let raw = call
                    .arguments
                    .get("todos")
                    .cloned()
                    .ok_or_else(|| SeamError::Invalid("`todos` is required".into()))?;
                let todos: Vec<Todo> = serde_json::from_value(raw)
                    .map_err(|e| SeamError::Invalid(format!("invalid todos: {e}")))?;
                let mut seen = std::collections::HashSet::new();
                for todo in &todos {
                    if todo.id.trim().is_empty() || todo.content.trim().is_empty() {
                        return Err(SeamError::Invalid(
                            "every todo needs a non-empty id and content".into(),
                        ));
                    }
                    if !STATUSES.contains(&todo.status.as_str()) {
                        return Err(SeamError::Invalid(format!(
                            "unknown todo status `{}` (expected one of {STATUSES:?})",
                            todo.status
                        )));
                    }
                    if !seen.insert(todo.id.clone()) {
                        return Err(SeamError::Invalid(format!(
                            "duplicate todo id `{}`",
                            todo.id
                        )));
                    }
                }
                let body =
                    serde_json::to_string(&todos).map_err(|e| SeamError::Invalid(e.to_string()))?;
                self.store
                    .memorize(
                        ContextFragment::new(session, FragmentKind::Note, body)
                            .with_key(todo_key(session)),
                    )
                    .await
                    .map_err(|e| SeamError::Io(e.to_string()))?;
                Ok(format!("recorded {} todo(s)", todos.len()))
            }
            "todo_read" => {
                let todos = self.read(session).await?;
                if todos.is_empty() {
                    return Ok("(no todos)".into());
                }
                Ok(todos
                    .into_iter()
                    .map(|t| format!("[{}] {} {}", t.status, t.id, t.content))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            other => Err(SeamError::Invalid(format!("unknown todo tool: {other}"))),
        }
    }
}

family_handler!(TodoToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::ToolHandler;

    async fn run(
        h: &TodoToolHandler,
        name: &str,
        args: serde_json::Value,
    ) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s1",
        )
        .await
    }

    #[tokio::test]
    async fn write_then_read_roundtrip() {
        let h = TodoToolHandler::new(agent_core::in_memory_context());
        assert_eq!(run(&h, "todo_read", json!({})).await.content, "(no todos)");
        let res = run(
            &h,
            "todo_write",
            json!({"todos": [
                {"id": "1", "content": "write code"},
                {"id": "2", "content": "test", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(res.content.contains("2 todo(s)"), "{}", res.content);
        let listed = run(&h, "todo_read", json!({})).await.content;
        assert!(listed.contains("[pending] 1 write code"), "{listed}");
        assert!(listed.contains("[in_progress] 2 test"), "{listed}");
    }

    #[tokio::test]
    async fn validation_rejects_bad_lists() {
        let h = TodoToolHandler::new(agent_core::in_memory_context());
        assert!(run(&h, "todo_write", json!({})).await.is_error);
        assert!(
            run(&h, "todo_write", json!({"todos": "nope"}))
                .await
                .is_error
        );
        assert!(run(
            &h,
            "todo_write",
            json!({"todos": [{"id": "1", "content": "x", "status": "weird"}]})
        )
        .await
        .content
        .contains("unknown todo status"));
        assert!(run(
            &h,
            "todo_write",
            json!({"todos": [
                {"id": "1", "content": "a"},
                {"id": "1", "content": "b"}
            ]})
        )
        .await
        .content
        .contains("duplicate todo id"));
        assert!(
            run(
                &h,
                "todo_write",
                json!({"todos": [{"id": "", "content": "a"}]})
            )
            .await
            .is_error
        );
        assert!(run(&h, "todo_nope", json!({})).await.is_error);
    }

    #[tokio::test]
    async fn corrupt_stored_state_is_reported() {
        let store = agent_core::in_memory_context();
        store
            .memorize(
                ContextFragment::new("s1", FragmentKind::Note, "{not json")
                    .with_key(todo_key("s1")),
            )
            .await
            .unwrap();
        let h = TodoToolHandler::new(store);
        let res = run(&h, "todo_read", json!({})).await;
        assert!(res.is_error);
        assert!(res.content.contains("corrupt"), "{}", res.content);
    }
}
