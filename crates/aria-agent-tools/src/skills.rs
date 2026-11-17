//! Skills tool family (`skill_*`).
//!
//! The catalog is what the prompt advertises; `skill_load` pulls the body in on
//! demand so a large catalog never inflates every request.

use crate::{arg_str, family_handler, Family};
use agent_core::seams::{SeamError, SkillsSeam};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

pub struct SkillsToolHandler {
    skills: Arc<dyn SkillsSeam>,
}

impl SkillsToolHandler {
    pub fn new(skills: Arc<dyn SkillsSeam>) -> Self {
        Self { skills }
    }
}

pub fn skills_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "skill_list".into(),
            description: "List the skills available in this deployment.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        Tool {
            name: "skill_load".into(),
            description: "Load the full body of a skill by name.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "name": { "type": "string" } },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
    ]
}

impl Family for SkillsToolHandler {
    fn prefix(&self) -> &'static str {
        "skill_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "skill_list" => {
                let catalog = self.skills.list().await?;
                if catalog.is_empty() {
                    return Ok("(no skills)".into());
                }
                Ok(catalog
                    .into_iter()
                    .map(|m| format!("{}: {} ({})", m.name, m.description, m.source))
                    .collect::<Vec<_>>()
                    .join("\n"))
            }
            "skill_load" => {
                let skill = self.skills.load(&arg_str(call, "name")?).await?;
                Ok(skill.body)
            }
            other => Err(SeamError::Invalid(format!("unknown skills tool: {other}"))),
        }
    }
}

family_handler!(SkillsToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{EmptySkills, FilesystemSkills};
    use agent_core::ToolHandler;

    async fn run(
        h: &SkillsToolHandler,
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
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn empty_catalog_and_missing_skill() {
        let h = SkillsToolHandler::new(Arc::new(EmptySkills));
        assert_eq!(
            run(&h, "skill_list", json!({})).await.content,
            "(no skills)"
        );
        let missing = run(&h, "skill_load", json!({"name": "x"})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("not_found"));
    }

    #[tokio::test]
    async fn filesystem_catalog_lists_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        let skill_dir = dir.path().join("rust");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Rust review\n\nBe thorough.\n",
        )
        .unwrap();
        let h = SkillsToolHandler::new(Arc::new(FilesystemSkills::new([dir.path().to_path_buf()])));
        let listed = run(&h, "skill_list", json!({})).await;
        assert!(listed.content.contains("rust: Rust review"));
        let loaded = run(&h, "skill_load", json!({"name": "rust"})).await;
        assert!(loaded.content.contains("Be thorough."));
        assert!(run(&h, "skill_load", json!({})).await.is_error);
    }
}
