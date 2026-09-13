//! Harness file persistence — the bridge between the in-memory [`Harness`]
//! (defined in `agent-core`) and the `.reef/` artifact directory that Git
//! versions. Each part is a plain Markdown file so diffs are human-readable.

use crate::error::ReefError;
use agent_core::{Harness, Rule, Skill};
use std::path::{Path, PathBuf};

const SYSTEM_FILE: &str = "system_prompt.md";
const SKILLS_DIR: &str = "skills";
const RULES_DIR: &str = "rules";

/// Write a harness to `dir` as Markdown files. Overwrites previous contents.
pub fn save(dir: &Path, harness: &Harness) -> Result<(), ReefError> {
    std::fs::create_dir_all(dir).map_err(|e| ReefError::Storage(e.to_string()))?;
    std::fs::write(dir.join(SYSTEM_FILE), &harness.system_prompt)
        .map_err(|e| ReefError::Storage(e.to_string()))?;

    write_parts(&dir.join(SKILLS_DIR), &harness.skills)?;
    write_parts(&dir.join(RULES_DIR), &harness.rules)?;
    Ok(())
}

fn write_parts(dir: &Path, parts: &[impl NamedPart]) -> Result<(), ReefError> {
    std::fs::create_dir_all(dir).map_err(|e| ReefError::Storage(e.to_string()))?;
    // Clear stale `*.md` so removed parts don't linger across saves.
    for entry in std::fs::read_dir(dir).map_err(|e| ReefError::Storage(e.to_string()))? {
        let entry = entry.map_err(|e| ReefError::Storage(e.to_string()))?;
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) == Some("md") {
            std::fs::remove_file(p).map_err(|e| ReefError::Storage(e.to_string()))?;
        }
    }
    for (i, part) in parts.iter().enumerate() {
        let fname = format!("{:02}_{}.md", i, safe_name(part.name()));
        std::fs::write(dir.join(fname), part.body())
            .map_err(|e| ReefError::Storage(e.to_string()))?;
    }
    Ok(())
}

/// Load a harness from `dir`. Returns [`ReefError::NotFound`] when the system
/// prompt file is missing (i.e. no harness has been persisted yet).
pub fn load(dir: &Path) -> Result<Harness, ReefError> {
    let system_path = dir.join(SYSTEM_FILE);
    if !system_path.exists() {
        return Err(ReefError::NotFound(format!(
            "no harness in {}",
            dir.display()
        )));
    }
    let system_prompt =
        std::fs::read_to_string(&system_path).map_err(|e| ReefError::Storage(e.to_string()))?;
    let skills = read_parts(&dir.join(SKILLS_DIR))?;
    let rules = read_parts(&dir.join(RULES_DIR))?;
    Ok(Harness {
        system_prompt,
        skills,
        rules,
    })
}

fn read_parts<T: PartFromName>(dir: &Path) -> Result<Vec<T>, ReefError> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| ReefError::Storage(e.to_string()))? {
        let entry = entry.map_err(|e| ReefError::Storage(e.to_string()))?;
        let p = entry.path();
        if p.extension().and_then(|e| e.to_str()) != Some("md") {
            continue;
        }
        let stem = p
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        // Strip a leading "NN_" index prefix if present.
        let name = stem
            .split_once('_')
            .map(|(pre, rest)| {
                if pre.chars().all(|c| c.is_ascii_digit()) {
                    rest.to_string()
                } else {
                    stem.clone()
                }
            })
            .unwrap_or(stem);
        let body = std::fs::read_to_string(&p).map_err(|e| ReefError::Storage(e.to_string()))?;
        out.push(T::from_name_body(name, body));
    }
    Ok(out)
}

/// Replace unsafe characters so a part name can be used in a filename.
fn safe_name(name: &str) -> String {
    let mut s: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() {
        s.push('_');
    }
    s
}

trait NamedPart {
    fn name(&self) -> &str;
    fn body(&self) -> &str;
}

impl NamedPart for Skill {
    fn name(&self) -> &str {
        &self.name
    }
    fn body(&self) -> &str {
        &self.body
    }
}

impl NamedPart for Rule {
    fn name(&self) -> &str {
        &self.name
    }
    fn body(&self) -> &str {
        &self.body
    }
}

trait PartFromName: Sized {
    fn from_name_body(name: String, body: String) -> Self;
}

impl PartFromName for Skill {
    fn from_name_body(name: String, body: String) -> Self {
        Skill { name, body }
    }
}

impl PartFromName for Rule {
    fn from_name_body(name: String, body: String) -> Self {
        Rule { name, body }
    }
}

/// Resolve the harness directory for a given root (e.g. `.reef/`).
pub fn harness_dir(root: &Path) -> PathBuf {
    root.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = std::env::temp_dir().join(format!("reef_harness_{}", uuid::Uuid::new_v4()));
        let h = Harness {
            system_prompt: "You are a bot.".into(),
            skills: vec![
                Skill {
                    name: "summarize".into(),
                    body: "condense".into(),
                },
                Skill {
                    name: "weird name!".into(),
                    body: "handled".into(),
                },
            ],
            rules: vec![Rule {
                name: "no_pii".into(),
                body: "never".into(),
            }],
        };
        save(&dir, &h).unwrap();
        let got = load(&dir).unwrap();
        assert_eq!(got.system_prompt, "You are a bot.");
        assert_eq!(got.skills.len(), 2);
        assert!(got
            .skills
            .iter()
            .any(|s| s.name == "summarize" && s.body == "condense"));
        assert!(got
            .skills
            .iter()
            .any(|s| s.name == "weird_name_" && s.body == "handled"));
        assert!(got
            .rules
            .iter()
            .any(|r| r.name == "no_pii" && r.body == "never"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_is_not_found() {
        let dir = std::env::temp_dir().join(format!("reef_missing_{}", uuid::Uuid::new_v4()));
        assert!(matches!(load(&dir), Err(ReefError::NotFound(_))));
    }

    #[test]
    fn load_empty_parts_ok() {
        let dir = std::env::temp_dir().join(format!("reef_empty_{}", uuid::Uuid::new_v4()));
        let h = Harness {
            system_prompt: "sys".into(),
            skills: vec![],
            rules: vec![],
        };
        save(&dir, &h).unwrap();
        let got = load(&dir).unwrap();
        assert!(got.skills.is_empty());
        assert!(got.rules.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_clears_stale_parts() {
        let dir = std::env::temp_dir().join(format!("reef_stale_{}", uuid::Uuid::new_v4()));
        // First save with a skill.
        save(
            &dir,
            &Harness {
                system_prompt: "sys".into(),
                skills: vec![Skill {
                    name: "old".into(),
                    body: "x".into(),
                }],
                rules: vec![],
            },
        )
        .unwrap();
        // Second save with no skills -> the stale file must be removed.
        save(
            &dir,
            &Harness {
                system_prompt: "sys".into(),
                skills: vec![],
                rules: vec![],
            },
        )
        .unwrap();
        let got = load(&dir).unwrap();
        assert!(got.skills.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
