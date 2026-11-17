//! Human question tool family (`ask_user`).
//!
//! `tool-ask-user` pauses on a provider-agnostic `ask()`. Without a provider the
//! call fails closed instead of returning an empty answer that looks human.

use crate::{arg_str, family_handler, Family};
use agent_core::seams::{SeamError, UserQuestions};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

pub struct AskToolHandler {
    questions: Arc<dyn UserQuestions>,
}

impl AskToolHandler {
    pub fn new(questions: Arc<dyn UserQuestions>) -> Self {
        Self { questions }
    }
}

pub fn ask_tools() -> Vec<Tool> {
    vec![Tool {
        name: "ask_user".into(),
        description: "Ask the human operator a question and wait for the answer.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "question": { "type": "string" },
                "options": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["question"],
            "additionalProperties": false
        }),
    }]
}

impl Family for AskToolHandler {
    fn prefix(&self) -> &'static str {
        "ask_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        if call.name != "ask_user" {
            return Err(SeamError::Invalid(format!(
                "unknown ask tool: {}",
                call.name
            )));
        }
        let question = arg_str(call, "question")?;
        let options: Vec<String> = call
            .arguments
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        self.questions.ask(&question, &options).await
    }
}

family_handler!(AskToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::NoQuestions;
    use agent_core::ToolHandler;
    use async_trait::async_trait;

    struct Fixed;

    #[async_trait]
    impl UserQuestions for Fixed {
        async fn ask(&self, question: &str, options: &[String]) -> Result<String, SeamError> {
            Ok(format!("{question}|{}", options.join(",")))
        }
    }

    async fn run(h: &AskToolHandler, args: serde_json::Value) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: "ask_user".into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn without_provider_it_fails_closed() {
        let h = AskToolHandler::new(Arc::new(NoQuestions));
        let res = run(&h, json!({"question": "which?"})).await;
        assert!(res.is_error);
        assert!(res.content.contains("unavailable"));
    }

    #[tokio::test]
    async fn forwards_question_and_options() {
        let h = AskToolHandler::new(Arc::new(Fixed));
        let res = run(&h, json!({"question": "env?", "options": ["dev", "prod"]})).await;
        assert_eq!(res.content, "env?|dev,prod");
        assert!(run(&h, json!({})).await.is_error);
    }

    #[test]
    fn schema_and_prefix_are_stable() {
        let tools = ask_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "ask_user");
        assert_eq!(tools[0].parameters["required"][0], "question");
        let h = AskToolHandler::new(Arc::new(Fixed));
        assert!(h.handles("ask_user"));
        assert!(!h.handles("other"));
    }

    #[tokio::test]
    async fn unknown_ask_tool_is_invalid() {
        let h = AskToolHandler::new(Arc::new(Fixed));
        let res = h
            .run(
                &ToolCall {
                    id: "c".into(),
                    name: "ask_other".into(),
                    arguments: json!({}),
                },
                &agent_core::in_memory_context(),
                "s",
            )
            .await;
        assert!(res.is_error);
        assert!(res.content.contains("invalid"), "{}", res.content);
    }
}
