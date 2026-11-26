//! Skill distillation: publishing a promoted playbook as a loadable `SKILL.md`.
//!
//! The skill catalog (`SkillsSeam` / `FilesystemSkills`) already scans
//! `<workspace>/.aria/skills/<name>/SKILL.md` and re-reads it on every call, so
//! writing that file is the whole integration — the next `skill_list` sees the
//! distilled skill. Nothing here touches a running prompt: the skill becomes
//! *available*, and the model chooses it on the next turn (ADR-0026, and the
//! reason hot-swap stays removed).
//!
//! Names are sanitised instead of trusted: the name reaches a filesystem path, so
//! a `../` must degrade to `__` rather than escape the skills root.

use agent_core::seams::{SeamError, SeamResult};
use async_trait::async_trait;
use std::path::{Path, PathBuf};

/// Where a distilled skill is written.
#[async_trait]
pub trait SkillSink: Send + Sync {
    /// Write (or overwrite) one skill. Must be idempotent.
    async fn write_skill(&self, name: &str, title: &str, body: &str) -> SeamResult<PathBuf>;
}

/// Writes `<root>/<name>/SKILL.md` under a single root.
#[derive(Debug, Clone)]
pub struct FilesystemSkillSink {
    root: PathBuf,
}

impl FilesystemSkillSink {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Resolve a skill name to its `SKILL.md`, refusing to leave `root`.
    pub fn path_for(&self, name: &str) -> SeamResult<PathBuf> {
        Ok(self.dir_for(name)?.join("SKILL.md"))
    }

    /// The skill's own directory under `root`.
    pub fn dir_for(&self, name: &str) -> SeamResult<PathBuf> {
        let sanitized: String = name
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let sanitized = sanitized.trim_matches('_');
        if sanitized.is_empty() {
            return Err(SeamError::Invalid(
                "skill name is empty after sanitising".into(),
            ));
        }
        Ok(self.root.join(sanitized))
    }
}

#[async_trait]
impl SkillSink for FilesystemSkillSink {
    async fn write_skill(&self, name: &str, title: &str, body: &str) -> SeamResult<PathBuf> {
        let dir = self.dir_for(name)?;
        let path = dir.join("SKILL.md");
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| SeamError::Io(format!("mkdir {}: {e}", dir.display())))?;
        let content = format!(
            "# {title}\n\n{body}\n\n\
             <!-- distilled by RSI; the playbook revision is the source of truth -->\n"
        );
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| SeamError::Io(format!("write {}: {e}", path.display())))?;
        Ok(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_sanitised_and_cannot_escape_the_root() {
        let sink = FilesystemSkillSink::new("/tmp/skills");
        assert_eq!(sink.root(), Path::new("/tmp/skills"));
        assert_eq!(
            sink.path_for("rsi-retry_storm-shell").unwrap(),
            PathBuf::from("/tmp/skills/rsi-retry_storm-shell/SKILL.md")
        );
        let escaped = sink.path_for("../../etc/passwd").unwrap();
        assert!(escaped.starts_with("/tmp/skills"));
        assert!(!escaped.to_string_lossy().contains(".."));
        assert!(matches!(sink.path_for(""), Err(SeamError::Invalid(_))));
        assert!(matches!(sink.path_for("../"), Err(SeamError::Invalid(_))));
    }

    #[tokio::test]
    async fn writing_a_skill_is_idempotent_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let sink = FilesystemSkillSink::new(dir.path().join(".aria/skills"));
        let path = sink
            .write_skill("rsi-retry-storm-shell", "Stop retrying", "read first")
            .await
            .unwrap();
        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.starts_with("# Stop retrying"));
        assert!(written.contains("read first"));
        assert!(written.contains("distilled by RSI"));

        // A second promotion of the same key overwrites instead of appending.
        sink.write_skill(
            "rsi-retry-storm-shell",
            "Stop retrying",
            "read the result first",
        )
        .await
        .unwrap();
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.contains("read the result first"));
        assert_eq!(rewritten.matches("# Stop retrying").count(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_skill_directory_is_an_explicit_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let sink = FilesystemSkillSink::new(dir.path().join("skills"));
        let skill_dir = sink.dir_for("locked").unwrap();
        std::fs::create_dir_all(&skill_dir).unwrap();
        // The directory exists but cannot be written into: `create_dir_all`
        // succeeds and the write itself fails.
        std::fs::set_permissions(&skill_dir, std::fs::Permissions::from_mode(0o555)).unwrap();
        let result = sink.write_skill("locked", "t", "b").await;
        std::fs::set_permissions(&skill_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(result, Err(SeamError::Io(_))), "{result:?}");
    }

    #[tokio::test]
    async fn an_unwritable_root_is_an_explicit_error() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, "a file").unwrap();
        let sink = FilesystemSkillSink::new(blocker.join("skills"));
        assert!(matches!(
            sink.write_skill("name", "t", "b").await,
            Err(SeamError::Io(_))
        ));
    }
}
