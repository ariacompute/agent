//! Skills seam.
//!
//! dsh merges several provider directories into one catalog (`ctx.skills`) and
//! `tool-skill` renders the catalog into the system prompt. Here the catalog is
//! also the source for the `prompt` seam's skills section.
//!
//! Name collisions across roots are an **error**, not a silent override: two
//! providers claiming the same skill name would make behaviour depend on scan
//! order.

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One catalog entry (cheap to render in a prompt).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// Where it came from (`"filesystem:/path"`, `"office"`, …).
    pub source: String,
}

/// A loaded skill body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub meta: SkillMeta,
    pub body: String,
}

#[async_trait]
pub trait SkillsSeam: Send + Sync {
    async fn list(&self) -> SeamResult<Vec<SkillMeta>>;
    async fn load(&self, name: &str) -> SeamResult<Skill>;
}

/// Provider for deployments without skills: empty catalog, `NotFound` on load.
#[derive(Debug, Clone, Copy, Default)]
pub struct EmptySkills;

#[async_trait]
impl SkillsSeam for EmptySkills {
    async fn list(&self) -> SeamResult<Vec<SkillMeta>> {
        Ok(Vec::new())
    }

    async fn load(&self, name: &str) -> SeamResult<Skill> {
        Err(SeamError::NotFound(format!("skill `{name}`")))
    }
}

/// Environment variable that pins the shared skill catalog.
pub const SKILLS_DIR_ENV: &str = "AGENT_RSI_SKILLS_DIR";

/// Home directory of the sidecar CLI (`ARIA_COMPUTE_HOME`).
pub const ARIA_HOME_ENV: &str = "ARIA_COMPUTE_HOME";

/// The one catalog every consumer reads.
///
/// RSI writes promoted skills here, the `skills` tool family lists/loads them
/// here, and every dsh session workspace links to it. One resolution order for
/// all three is what keeps a distilled skill from being written where nothing
/// looks for it.
///
/// Order: `AGENT_RSI_SKILLS_DIR` → `$ARIA_COMPUTE_HOME/skills` →
/// `$HOME/.ariacompute/skills` → `.ariacompute/skills` when no home is known.
pub fn shared_skill_catalog() -> PathBuf {
    skill_catalog_from(
        std::env::var_os(SKILLS_DIR_ENV),
        std::env::var_os(ARIA_HOME_ENV),
        std::env::var_os("HOME"),
    )
}

/// Pure resolution behind [`shared_skill_catalog`], testable without touching
/// the process environment.
pub fn skill_catalog_from(
    explicit: Option<std::ffi::OsString>,
    aria_home: Option<std::ffi::OsString>,
    home: Option<std::ffi::OsString>,
) -> PathBuf {
    fn usable(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
        value
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    }
    if let Some(explicit) = usable(explicit) {
        return explicit;
    }
    if let Some(home) = usable(aria_home) {
        return home.join("skills");
    }
    if let Some(home) = usable(home) {
        return home.join(".ariacompute/skills");
    }
    PathBuf::from(".ariacompute/skills")
}

/// Provider that reads `<root>/<name>/SKILL.md` from each root.
#[derive(Debug, Clone, Default)]
pub struct FilesystemSkills {
    roots: Vec<PathBuf>,
}

impl FilesystemSkills {
    pub fn new(roots: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            roots: roots.into_iter().collect(),
        }
    }

    fn scan(&self) -> SeamResult<Vec<(SkillMeta, PathBuf)>> {
        let mut out: Vec<(SkillMeta, PathBuf)> = Vec::new();
        for root in &self.roots {
            let entries = match std::fs::read_dir(root) {
                Ok(rd) => rd,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(SeamError::Io(format!("scan {}: {e}", root.display()))),
            };
            for entry in entries.flatten() {
                let dir = entry.path();
                if !dir.is_dir() {
                    continue;
                }
                let file = dir.join("SKILL.md");
                if !file.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().to_string();
                let description = read_description(&file);
                let meta = SkillMeta {
                    name,
                    description,
                    source: format!("filesystem:{}", root.display()),
                };
                if let Some((existing, _)) = out.iter().find(|(m, _)| m.name == meta.name) {
                    return Err(SeamError::Invalid(format!(
                        "skill `{}` is provided by both {} and {}",
                        meta.name, existing.source, meta.source
                    )));
                }
                out.push((meta, file));
            }
        }
        out.sort_by(|a, b| a.0.name.cmp(&b.0.name));
        Ok(out)
    }
}

fn read_description(path: &std::path::Path) -> String {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|body| {
            body.lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .map(|l| l.trim_start_matches('#').trim().to_string())
        })
        .unwrap_or_default()
}

fn read_skill_body(path: &std::path::Path) -> SeamResult<String> {
    std::fs::read_to_string(path)
        .map_err(|e| SeamError::Io(format!("read {}: {e}", path.display())))
}

#[async_trait]
impl SkillsSeam for FilesystemSkills {
    async fn list(&self) -> SeamResult<Vec<SkillMeta>> {
        Ok(self.scan()?.into_iter().map(|(m, _)| m).collect())
    }

    async fn load(&self, name: &str) -> SeamResult<Skill> {
        let found = self
            .scan()?
            .into_iter()
            .find(|(m, _)| m.name == name)
            .ok_or_else(|| SeamError::NotFound(format!("skill `{name}`")))?;
        let body = read_skill_body(&found.1)?;
        Ok(Skill {
            meta: found.0,
            body,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &std::path::Path, name: &str, description: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), format!("# {description}\n\nbody\n")).unwrap();
    }

    #[tokio::test]
    async fn empty_provider_is_fail_closed() {
        let skills = EmptySkills;
        assert!(skills.list().await.unwrap().is_empty());
        assert!(matches!(
            skills.load("x").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn filesystem_provider_lists_and_loads() {
        let dir = tempfile::tempdir().unwrap();
        write_skill(dir.path(), "rust", "Rust review");
        write_skill(dir.path(), "sql", "SQL review");
        let skills = FilesystemSkills::new([dir.path().to_path_buf()]);
        let list = skills.list().await.unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].name, "rust");
        assert_eq!(list[0].description, "Rust review");
        let loaded = skills.load("rust").await.unwrap();
        assert!(loaded.body.contains("body"));
        assert!(matches!(
            skills.load("nope").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn missing_root_is_skipped() {
        let skills = FilesystemSkills::new([PathBuf::from("/definitely/not/here")]);
        assert!(skills.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scan_skips_non_skills_and_reports_bad_roots() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("loose.txt"), "not a skill").unwrap();
        std::fs::create_dir(dir.path().join("empty")).unwrap();
        let skills = FilesystemSkills::new([dir.path().to_path_buf()]);
        assert!(skills.list().await.unwrap().is_empty());

        let blocker = tempfile::tempdir().unwrap();
        let file = blocker.path().join("file");
        std::fs::write(&file, "x").unwrap();
        let skills = FilesystemSkills::new([file]);
        assert!(matches!(skills.list().await, Err(SeamError::Io(_))));
    }

    #[test]
    fn unreadable_skill_body_is_an_io_error() {
        // Reading a directory is a real IO error on every platform and does
        // not depend on chmod behaviour under root.
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(read_skill_body(dir.path()), Err(SeamError::Io(_))));
    }

    #[tokio::test]
    async fn duplicate_skill_names_across_roots_are_rejected() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_skill(a.path(), "dup", "one");
        write_skill(b.path(), "dup", "two");
        let skills = FilesystemSkills::new([a.path().to_path_buf(), b.path().to_path_buf()]);
        let err = skills.list().await.unwrap_err();
        assert!(matches!(err, SeamError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn the_shared_catalog_prefers_the_explicit_override() {
        let resolved = skill_catalog_from(
            Some("/srv/catalog".into()),
            Some("/home/u/.ariacompute".into()),
            Some("/home/u".into()),
        );
        assert_eq!(resolved, PathBuf::from("/srv/catalog"));
    }

    #[test]
    fn the_shared_catalog_falls_back_to_the_aria_home_then_home() {
        assert_eq!(
            skill_catalog_from(None, Some("/srv/aria".into()), Some("/home/u".into())),
            PathBuf::from("/srv/aria/skills")
        );
        assert_eq!(
            skill_catalog_from(None, None, Some("/home/u".into())),
            PathBuf::from("/home/u/.ariacompute/skills")
        );
        // An empty value is "unset", not "the current directory".
        assert_eq!(
            skill_catalog_from(Some("".into()), Some("".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.ariacompute/skills")
        );
        // No home at all still yields a relative, writable location.
        assert_eq!(
            skill_catalog_from(None, None, None),
            PathBuf::from(".ariacompute/skills")
        );
    }

    #[test]
    fn the_environment_backed_catalog_never_resolves_to_nothing() {
        // Only "it resolves" is asserted: the test must not depend on the host's
        // `HOME` or on an `AGENT_RSI_SKILLS_DIR` the runner may have exported.
        assert!(!shared_skill_catalog().as_os_str().is_empty());
    }
}
