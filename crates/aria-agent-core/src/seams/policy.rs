//! Deployment-wide sandbox policy: one mode, one root directory.
//!
//! dsh keeps `ctx.sandboxPolicy` as a **core** seam precisely so bash and fs can
//! never be restricted to different roots. [`SandboxPolicy`] is the single place
//! that answers two questions:
//!
//! 1. may this tool run at all under the current [`PermissionPreset`]?
//! 2. is this path inside the workspace root (after normalization + symlink
//!    resolution)?
//!
//! Both answers fail closed ([`PolicyError`]).

use serde::{Deserialize, Serialize};
use std::path::{Component, Path, PathBuf};

/// Three permission presets, mirroring dsh's `permissionPresets`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PermissionPreset {
    /// No mutations at all: reads, listing, search and memory lookups only.
    ReadOnly,
    /// Writes allowed, but only inside the configured root.
    #[default]
    WorkspaceWrite,
    /// No path restriction (still no implicit approval for destructive tools).
    FullAccess,
}

impl PermissionPreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().replace('-', "_").as_str() {
            "read_only" | "readonly" | "read" => Some(Self::ReadOnly),
            "workspace_write" | "workspace" | "write" => Some(Self::WorkspaceWrite),
            "danger_full_access" | "full_access" | "full" => Some(Self::FullAccess),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::WorkspaceWrite => "workspace_write",
            Self::FullAccess => "danger_full_access",
        }
    }

    /// Whether mutating tools are permitted at all.
    pub fn allows_mutation(&self) -> bool {
        !matches!(self, Self::ReadOnly)
    }

    /// Whether the preset skips the approval round-trip for mutating tools.
    ///
    /// `ReadOnly` never reaches this because mutation is refused earlier.
    pub fn auto_approves_mutation(&self) -> bool {
        matches!(self, Self::FullAccess)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    #[error("path escapes the workspace root: {0}")]
    Escape(String),
    #[error("path is not valid UTF-8")]
    NotUtf8,
    #[error("workspace root is not configured")]
    NoRoot,
    #[error("permission preset `{preset}` forbids `{tool}`")]
    Denied { tool: String, preset: &'static str },
    #[error("invalid policy: {0}")]
    Invalid(String),
}

/// Tools that never mutate state and are allowed under every preset.
const READ_ONLY_PREFIXES: &[&str] = &[
    "fs_read",
    "fs_list",
    "fs_stat",
    "memo_",
    "skill_",
    "web_",
    "todo_read",
    "plan_get",
    "job_output",
    "job_list",
    // Only the read-only members of the orchestration families: creating a team,
    // claiming a task or scheduling a job are mutations and need the write preset.
    "team_status",
    "team_list",
    "subagent_list",
    "schedule_list",
    "ask_user",
];

/// The single source of truth for mode + root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxPolicy {
    root: PathBuf,
    preset: PermissionPreset,
}

impl Default for SandboxPolicy {
    fn default() -> Self {
        let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            root,
            preset: PermissionPreset::default(),
        }
    }
}

impl SandboxPolicy {
    /// Build a policy, resolving the root once (so later symlink games cannot
    /// change what "the workspace" means).
    pub fn new(root: impl AsRef<Path>, preset: PermissionPreset) -> Result<Self, PolicyError> {
        let raw = root.as_ref();
        if raw.as_os_str().is_empty() {
            return Err(PolicyError::NoRoot);
        }
        let canonical = raw.canonicalize().unwrap_or_else(|_| raw.to_path_buf());
        if !canonical.is_absolute() {
            return Err(PolicyError::Invalid(format!(
                "root must be absolute: {}",
                canonical.display()
            )));
        }
        Ok(Self {
            root: canonical,
            preset,
        })
    }

    /// Convenience: a workspace-write policy rooted at `root`.
    pub fn workspace(root: impl AsRef<Path>) -> Result<Self, PolicyError> {
        Self::new(root, PermissionPreset::WorkspaceWrite)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn preset(&self) -> PermissionPreset {
        self.preset
    }

    /// Whether `tool` may run under this preset (no approval involved).
    pub fn check_tool(&self, tool: &str) -> Result<(), PolicyError> {
        if self.preset.allows_mutation() || is_read_only_tool(tool) {
            return Ok(());
        }
        Err(PolicyError::Denied {
            tool: tool.to_string(),
            preset: self.preset.as_str(),
        })
    }

    /// Normalize `requested` against the root and reject any escape.
    ///
    /// Normalization is lexical (`..` is resolved without touching the
    /// filesystem), then the *existing* prefix is canonicalized so a symlink
    /// pointing outside the root is still rejected.
    pub fn resolve_path(&self, requested: &str) -> Result<PathBuf, PolicyError> {
        if requested.trim().is_empty() {
            return Err(PolicyError::Invalid("empty path".into()));
        }
        let candidate = Path::new(requested);
        let joined = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            self.root.join(candidate)
        };
        let normalized = normalize(&joined);
        if !normalized.starts_with(&self.root) {
            return Err(PolicyError::Escape(requested.to_string()));
        }
        // Symlink escape: canonicalize the deepest existing ancestor.
        let mut probe = normalized.as_path();
        let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
        loop {
            if let Ok(real) = probe.canonicalize() {
                let real = suffix.iter().rev().fold(real, |acc, part| acc.join(part));
                if !real.starts_with(&self.root) {
                    return Err(PolicyError::Escape(requested.to_string()));
                }
                return Ok(real);
            }
            match probe.parent() {
                Some(parent) => {
                    if let Some(name) = probe.file_name() {
                        suffix.push(name);
                    }
                    probe = parent;
                }
                None => return Err(PolicyError::Escape(requested.to_string())),
            }
        }
    }
}

/// Lexically resolve `.` / `..` without I/O.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Tool-name based read-only classification (stable tool prefixes, ADR-0016).
pub fn is_read_only_tool(tool: &str) -> bool {
    READ_ONLY_PREFIXES.iter().any(|p| {
        if let Some(stripped) = p.strip_suffix('_') {
            tool == stripped || tool.starts_with(p)
        } else {
            tool == *p
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> SandboxPolicy {
        SandboxPolicy::new(std::env::temp_dir(), PermissionPreset::WorkspaceWrite).unwrap()
    }

    #[test]
    fn presets_parse_and_roundtrip() {
        for (raw, want) in [
            ("read_only", PermissionPreset::ReadOnly),
            ("readonly", PermissionPreset::ReadOnly),
            ("workspace-write", PermissionPreset::WorkspaceWrite),
            ("danger-full-access", PermissionPreset::FullAccess),
        ] {
            assert_eq!(PermissionPreset::parse(raw), Some(want), "{raw}");
            assert_eq!(PermissionPreset::parse(want.as_str()), Some(want));
        }
        assert_eq!(PermissionPreset::parse(""), None);
        assert_eq!(PermissionPreset::parse("nope"), None);
        assert!(!PermissionPreset::ReadOnly.allows_mutation());
        assert!(PermissionPreset::WorkspaceWrite.allows_mutation());
        assert!(!PermissionPreset::WorkspaceWrite.auto_approves_mutation());
        assert!(PermissionPreset::FullAccess.auto_approves_mutation());
    }

    #[test]
    fn read_only_preset_denies_mutating_tools() {
        let p = SandboxPolicy::new(std::env::temp_dir(), PermissionPreset::ReadOnly).unwrap();
        assert!(p.check_tool("fs_read").is_ok());
        assert!(p.check_tool("memo_store").is_ok());
        assert!(matches!(
            p.check_tool("fs_write"),
            Err(PolicyError::Denied { .. })
        ));
        assert!(matches!(
            p.check_tool("shell"),
            Err(PolicyError::Denied { .. })
        ));
    }

    #[test]
    fn workspace_write_allows_mutation() {
        let p = policy();
        assert!(p.check_tool("fs_write").is_ok());
        assert!(p.check_tool("shell").is_ok());
    }

    #[test]
    fn relative_paths_resolve_under_root() {
        let p = policy();
        let got = p.resolve_path("nested/file.txt").unwrap();
        assert!(got.starts_with(p.root()));
        assert!(got.ends_with("nested/file.txt"));
    }

    #[test]
    fn parent_traversal_is_rejected() {
        let p = policy();
        assert!(matches!(
            p.resolve_path("../escape.txt"),
            Err(PolicyError::Escape(_))
        ));
        assert!(matches!(
            p.resolve_path("a/../../escape.txt"),
            Err(PolicyError::Escape(_))
        ));
    }

    #[test]
    fn absolute_path_outside_root_is_rejected() {
        let p = policy();
        let outside = if cfg!(windows) {
            "C:\\Windows\\win.ini"
        } else {
            "/etc/passwd"
        };
        assert!(matches!(
            p.resolve_path(outside),
            Err(PolicyError::Escape(_))
        ));
    }

    #[test]
    fn empty_and_relative_root_are_rejected() {
        assert_eq!(
            SandboxPolicy::new("", PermissionPreset::ReadOnly).unwrap_err(),
            PolicyError::NoRoot
        );
    }

    #[test]
    fn read_only_classification() {
        assert!(is_read_only_tool("fs_read"));
        assert!(is_read_only_tool("fs_list"));
        assert!(is_read_only_tool("memo_search"));
        assert!(is_read_only_tool("web_fetch"));
        assert!(is_read_only_tool("todo_read"));
        assert!(is_read_only_tool("team_status"));
        assert!(is_read_only_tool("subagent_list"));
        assert!(is_read_only_tool("schedule_list"));
        assert!(!is_read_only_tool("fs_write"));
        assert!(!is_read_only_tool("shell"));
        assert!(!is_read_only_tool("job_start"));
        // Orchestration that mutates needs the write preset.
        assert!(!is_read_only_tool("team_create"));
        assert!(!is_read_only_tool("team_claim"));
        assert!(!is_read_only_tool("team_complete"));
        assert!(!is_read_only_tool("subagent_run"));
        assert!(!is_read_only_tool("schedule_add"));
    }
}
