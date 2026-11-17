//! Plan tool family (`plan_*`).
//!
//! dsh keeps plan state in `ctx.planMode` and refreshes it at turn boundaries.
//! Here the plan is a keyed `Note` fragment on the storage seam, so it survives
//! restarts with the rest of the context and needs no extra table.

use crate::{arg_str, family_handler, Family};
use agent_core::context::{ContextFragment, ContextStore, FragmentKind};
use agent_core::seams::SeamError;
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

/// Key under which the plan is stored for a session.
pub fn plan_key(session: &str) -> String {
    format!("plan:{session}")
}

pub struct PlanToolHandler {
    store: Arc<dyn ContextStore>,
}

impl PlanToolHandler {
    pub fn new(store: Arc<dyn ContextStore>) -> Self {
        Self { store }
    }
}

pub fn plan_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "plan_set".into(),
            description: "Record or replace the current plan for this session.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "plan": { "type": "string" } },
                "required": ["plan"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "plan_get".into(),
            description: "Read the current plan.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for PlanToolHandler {
    fn prefix(&self) -> &'static str {
        "plan_"
    }

    async fn dispatch(&self, call: &ToolCall, session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "plan_set" => {
                let plan = arg_str(call, "plan")?;
                self.store
                    .memorize(
                        ContextFragment::new(session, FragmentKind::Note, plan.clone())
                            .with_key(plan_key(session)),
                    )
                    .await
                    .map_err(|e| SeamError::Io(e.to_string()))?;
                Ok(format!("plan recorded ({} bytes)", plan.len()))
            }
            "plan_get" => match self
                .store
                .get_by_key(session, &plan_key(session))
                .await
                .map_err(|e| SeamError::Io(e.to_string()))?
            {
                Some(f) => Ok(f.content),
                None => Ok("(no plan)".into()),
            },
            other => Err(SeamError::Invalid(format!("unknown plan tool: {other}"))),
        }
    }
}

family_handler!(PlanToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remember;
    use agent_core::ToolHandler;

    async fn run(
        h: &PlanToolHandler,
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
    async fn set_then_get_roundtrip_is_session_scoped() {
        let store = agent_core::in_memory_context();
        let h = PlanToolHandler::new(store.clone());
        assert_eq!(run(&h, "plan_get", json!({})).await.content, "(no plan)");
        assert!(run(&h, "plan_set", json!({"plan": "1. ship"}))
            .await
            .content
            .contains("7 bytes"));
        assert_eq!(run(&h, "plan_get", json!({})).await.content, "1. ship");
        assert!(matches!(
            store.get_by_key("other", &plan_key("s1")).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn bad_arguments_are_invalid() {
        let h = PlanToolHandler::new(agent_core::in_memory_context());
        assert!(run(&h, "plan_set", json!({})).await.is_error);
        assert!(run(&h, "plan_nope", json!({})).await.is_error);
    }

    #[tokio::test]
    async fn remember_is_independent_of_the_plan_key() {
        let store = agent_core::in_memory_context();
        remember(&store, "s1", "tool output").await;
        assert!(store
            .get_by_key("s1", &plan_key("s1"))
            .await
            .unwrap()
            .is_none());
    }
}
