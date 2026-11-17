//! Shell tool family (`shell`).
//!
//! Accepts an **argv array only** — no shell-string interpolation — and hands it
//! to the `shell` seam, which runs it in the configured sandbox.

use crate::{arg_str_opt, family_handler, Family};
use agent_core::seams::{SeamError, ShellSeam};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

pub struct ShellToolHandler {
    shell: Arc<dyn ShellSeam>,
}

impl ShellToolHandler {
    pub fn new(shell: Arc<dyn ShellSeam>) -> Self {
        Self { shell }
    }
}

pub fn shell_tools() -> Vec<Tool> {
    vec![Tool {
        name: "shell".into(),
        description:
            "Execute a command given as a [program, ...args] array inside the sandbox and return its combined output."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Program followed by its arguments, e.g. [\"ls\", \"-la\"]."
                },
                "cwd": {
                    "type": "string",
                    "description": "Optional working directory inside the workspace."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }),
    }]
}

impl Family for ShellToolHandler {
    fn prefix(&self) -> &'static str {
        "shell"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        if call.name != "shell" {
            return Err(SeamError::Invalid(format!(
                "unknown shell tool: {}",
                call.name
            )));
        }
        let argv: Vec<String> = call
            .arguments
            .get("command")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .ok_or_else(|| SeamError::Invalid("`command` must be an array of strings".into()))?;
        let cwd = arg_str_opt(call, "cwd");
        let out = self.shell.exec(&argv, cwd.as_deref()).await?;
        Ok(out.render())
    }
}

family_handler!(ShellToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::ShellOutput;
    use agent_core::ToolHandler;
    use async_trait::async_trait;

    struct Fake {
        out: ShellOutput,
        seen: std::sync::Mutex<Option<(Vec<String>, Option<String>)>>,
    }

    #[async_trait]
    impl ShellSeam for Fake {
        async fn exec(&self, argv: &[String], cwd: Option<&str>) -> Result<ShellOutput, SeamError> {
            *self.seen.lock().unwrap() = Some((argv.to_vec(), cwd.map(str::to_string)));
            Ok(self.out.clone())
        }
    }

    fn fake(code: i64) -> Arc<Fake> {
        Arc::new(Fake {
            out: ShellOutput {
                exit_code: code,
                stdout: "out".into(),
                stderr: "err".into(),
            },
            seen: std::sync::Mutex::new(None),
        })
    }

    async fn run(h: &ShellToolHandler, args: serde_json::Value) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: "shell".into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn forwards_argv_and_cwd() {
        let fake = fake(0);
        let h = ShellToolHandler::new(fake.clone());
        let res = run(&h, json!({"command": ["ls", "-la"], "cwd": "/w"})).await;
        assert!(!res.is_error);
        assert!(res.content.contains("exit=0"));
        let seen = fake.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.0, vec!["ls".to_string(), "-la".to_string()]);
        assert_eq!(seen.1.as_deref(), Some("/w"));
    }

    #[tokio::test]
    async fn bad_arguments_are_invalid_not_executed() {
        let fake = fake(0);
        let h = ShellToolHandler::new(fake.clone());
        assert!(run(&h, json!({})).await.content.contains("invalid"));
        assert!(run(&h, json!({"command": "ls"})).await.is_error);
        assert!(fake.seen.lock().unwrap().is_none());
    }

    #[test]
    fn schema_requires_command_array() {
        let tools = shell_tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].parameters["required"][0], "command");
        assert_eq!(
            tools[0].parameters["properties"]["command"]["type"],
            "array"
        );
    }

    #[tokio::test]
    async fn unknown_shell_tool_is_invalid() {
        let h = ShellToolHandler::new(fake(0));
        let res = h
            .run(
                &ToolCall {
                    id: "c".into(),
                    name: "shell_other".into(),
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
