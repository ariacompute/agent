//! System-prompt assembly.
//!
//! dsh's `ctx.systemPrompt` is a **core** seam: exactly one place owns the text
//! the model sees. Sections are assembled in a stable order, deduplicated by id,
//! and truncated against a byte budget so a huge skill catalog can never crowd
//! out the identity/instructions (ADR-0015).

use crate::seams::policy::PermissionPreset;
use crate::seams::SkillMeta;

/// One prompt segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSection {
    /// Stable identifier; a later section with the same id replaces the earlier.
    pub id: &'static str,
    /// Lower sorts first.
    pub priority: u8,
    pub body: String,
}

impl PromptSection {
    pub fn new(id: &'static str, priority: u8, body: impl Into<String>) -> Self {
        Self {
            id,
            priority,
            body: body.into(),
        }
    }
}

/// Default byte budget for the assembled prompt (8 KiB).
pub const DEFAULT_PROMPT_BUDGET: usize = 8 * 1024;

/// Ordered, budgeted prompt builder.
#[derive(Debug, Clone)]
pub struct SystemPrompt {
    sections: Vec<PromptSection>,
    budget_bytes: usize,
}

impl Default for SystemPrompt {
    fn default() -> Self {
        Self::new(DEFAULT_PROMPT_BUDGET)
    }
}

impl SystemPrompt {
    pub fn new(budget_bytes: usize) -> Self {
        Self {
            sections: Vec::new(),
            budget_bytes,
        }
    }

    /// Add a section. Empty bodies are ignored so conditional sections can be
    /// pushed unconditionally.
    pub fn push(&mut self, section: PromptSection) -> &mut Self {
        if !section.body.trim().is_empty() {
            self.sections.push(section);
        }
        self
    }

    /// Convenience for optional sections.
    pub fn push_optional(&mut self, section: Option<PromptSection>) -> &mut Self {
        if let Some(s) = section {
            self.push(s);
        }
        self
    }

    pub fn sections(&self) -> &[PromptSection] {
        &self.sections
    }

    /// Stable assembly: sort by `(priority, id)`, keep the last section per id.
    pub fn build(&self) -> String {
        let mut ordered = self.sections.clone();
        ordered.sort_by(|a, b| a.priority.cmp(&b.priority).then(a.id.cmp(b.id)));
        let mut deduped: Vec<PromptSection> = Vec::with_capacity(ordered.len());
        for section in ordered {
            if let Some(existing) = deduped.iter_mut().find(|s| s.id == section.id) {
                *existing = section;
            } else {
                deduped.push(section);
            }
        }
        let joined = deduped
            .iter()
            .map(|s| s.body.trim())
            .collect::<Vec<_>>()
            .join("\n\n");
        truncate(&joined, self.budget_bytes)
    }
}

fn truncate(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_string();
    }
    let mut cut = budget;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}\n[prompt truncated]", &text[..cut])
}

/// Base identity section (always first).
pub fn identity(name: &str) -> PromptSection {
    PromptSection::new("identity", 0, format!("You are {name}."))
}

/// User-supplied instructions.
pub fn instructions(text: &str) -> PromptSection {
    PromptSection::new("instructions", 10, text)
}

/// The single execution directory.
pub fn working_directory(path: &str) -> PromptSection {
    PromptSection::new(
        "working_directory",
        20,
        format!("Working directory: {path}"),
    )
}

/// Current permission preset, so the model knows what it may attempt.
pub fn permissions(preset: PermissionPreset) -> PromptSection {
    PromptSection::new(
        "permissions",
        30,
        format!(
            "Permission preset: {}. {}",
            preset.as_str(),
            match preset {
                PermissionPreset::ReadOnly =>
                    "Read-only: never write files or run mutating commands.",
                PermissionPreset::WorkspaceWrite => {
                    "Writes are limited to the working directory."
                }
                PermissionPreset::FullAccess =>
                    "Full access: be extra careful with destructive commands.",
            }
        ),
    )
}

/// Bounded skill catalog (name + one-line description).
pub fn skills(catalog: &[SkillMeta]) -> PromptSection {
    if catalog.is_empty() {
        return PromptSection::new("skills", 40, "");
    }
    let mut body = String::from("Available skills (load with `skill_load`):");
    for meta in catalog {
        body.push_str(&format!("\n- {}: {}", meta.name, meta.description));
    }
    PromptSection::new("skills", 40, body)
}

/// Current plan, when plan mode is active.
pub fn plan(text: &str) -> PromptSection {
    PromptSection::new("plan", 50, format!("Current plan:\n{text}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_are_ordered_and_empty_ones_dropped() {
        let mut p = SystemPrompt::default();
        p.push(permissions(PermissionPreset::ReadOnly));
        p.push(skills(&[]));
        p.push(identity("Aria"));
        p.push(instructions("be terse"));
        let out = p.build();
        assert!(out.starts_with("You are Aria."));
        assert!(out.find("be terse").unwrap() < out.find("Permission preset").unwrap());
        assert!(!out.contains("Available skills"));
    }

    #[test]
    fn duplicate_ids_keep_the_last_body() {
        let mut p = SystemPrompt::default();
        p.push(PromptSection::new("x", 1, "first"));
        p.push(PromptSection::new("x", 1, "second"));
        let out = p.build();
        assert!(!out.contains("first"));
        assert!(out.contains("second"));
        assert_eq!(p.sections().len(), 2);
    }

    #[test]
    fn budget_truncation_is_char_safe() {
        let mut p = SystemPrompt::new(16);
        p.push(identity("éééééééééééé"));
        let out = p.build();
        assert!(out.len() <= 16 + "[prompt truncated]".len() + 1);
        assert!(out.ends_with("[prompt truncated]"));
    }

    #[test]
    fn skill_catalog_and_plan_render() {
        let metas = vec![SkillMeta {
            name: "rust".into(),
            description: "Rust review".into(),
            source: "fs".into(),
        }];
        let mut p = SystemPrompt::default();
        p.push(skills(&metas));
        p.push_optional(Some(plan("1. do it")));
        p.push_optional(None);
        let out = p.build();
        assert!(out.contains("- rust: Rust review"));
        assert!(out.contains("Current plan:\n1. do it"));
        assert_eq!(p.sections().len(), 2);
    }

    #[test]
    fn working_directory_and_permissions_render() {
        let mut p = SystemPrompt::default();
        p.push(working_directory("/w"));
        p.push(permissions(PermissionPreset::WorkspaceWrite));
        let out = p.build();
        assert!(out.contains("Working directory: /w"));
        assert!(out.contains("workspace_write"));
        assert!(out.contains("limited to the working directory"));
    }
}
