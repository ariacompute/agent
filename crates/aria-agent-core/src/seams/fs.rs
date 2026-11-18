//! Filesystem seam: every path goes through the single [`SandboxPolicy`] root.
//!
//! dsh exposes exactly the primitives tool-fs needs (`ctx.fs`) and keeps the
//! policy out of the tool. Same here: [`LocalFs`] is the local provider and it
//! refuses anything the policy rejects — there is no bypass flag.

use super::policy::{PolicyError, SandboxPolicy};
use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Metadata for `fs_stat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStat {
    pub path: String,
    pub is_dir: bool,
    pub len: u64,
    /// Unix epoch milliseconds, if the platform reports it.
    pub modified_ms: Option<i64>,
}

#[async_trait]
pub trait FsSeam: Send + Sync {
    async fn read(&self, path: &str) -> SeamResult<String>;
    async fn write(&self, path: &str, contents: &str) -> SeamResult<()>;
    /// Replace the first `old` occurrence with `new`, returning the replacement
    /// count (0 is an error: the caller asked for an edit that cannot apply).
    async fn edit(&self, path: &str, old: &str, new: &str) -> SeamResult<usize>;
    async fn list(&self, path: &str) -> SeamResult<Vec<String>>;
    async fn stat(&self, path: &str) -> SeamResult<FileStat>;
}

/// Local provider rooted at [`SandboxPolicy::root`].
pub struct LocalFs {
    policy: Arc<SandboxPolicy>,
}

impl LocalFs {
    pub fn new(policy: Arc<SandboxPolicy>) -> Self {
        Self { policy }
    }

    pub fn policy(&self) -> &SandboxPolicy {
        &self.policy
    }

    fn resolve(&self, path: &str) -> SeamResult<std::path::PathBuf> {
        self.policy.resolve_path(path).map_err(map_policy)
    }
}

fn map_policy(e: PolicyError) -> SeamError {
    match e {
        PolicyError::Escape(p) => {
            SeamError::Denied(format!("path escapes the workspace root: {p}"))
        }
        PolicyError::NoRoot => SeamError::Unavailable("no workspace root configured".into()),
        other => SeamError::Invalid(other.to_string()),
    }
}

fn io_err(what: &str, path: &std::path::Path, e: std::io::Error) -> SeamError {
    SeamError::Io(format!("{what} {}: {e}", path.display()))
}

/// A plain (non-capturing) mapper for the rare `next_entry` IO failure.
fn list_err(e: std::io::Error) -> SeamError {
    SeamError::Io(format!("list directory: {e}"))
}

async fn write_file(path: &std::path::Path, contents: &str) -> SeamResult<()> {
    tokio::fs::write(path, contents)
        .await
        .map_err(|e| io_err("write", path, e))
}

#[async_trait]
impl FsSeam for LocalFs {
    async fn read(&self, path: &str) -> SeamResult<String> {
        let p = self.resolve(path)?;
        tokio::fs::read_to_string(&p)
            .await
            .map_err(|e| SeamError::Io(format!("read {}: {e}", p.display())))
    }

    async fn write(&self, path: &str, contents: &str) -> SeamResult<()> {
        let p = self.resolve(path)?;
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| io_err("mkdir", parent, e))?;
        }
        write_file(&p, contents).await
    }

    async fn edit(&self, path: &str, old: &str, new: &str) -> SeamResult<usize> {
        if old.is_empty() {
            return Err(SeamError::Invalid("`old` must not be empty".into()));
        }
        let p = self.resolve(path)?;
        let body = tokio::fs::read_to_string(&p)
            .await
            .map_err(|e| io_err("read", &p, e))?;
        if !body.contains(old) {
            return Err(SeamError::NotFound(format!(
                "`old` text not found in {}",
                p.display()
            )));
        }
        let updated = body.replacen(old, new, 1);
        write_file(&p, &updated).await?;
        Ok(1)
    }

    async fn list(&self, path: &str) -> SeamResult<Vec<String>> {
        let p = self.resolve(path)?;
        let mut out = Vec::new();
        let mut rd = tokio::fs::read_dir(&p)
            .await
            .map_err(|e| io_err("list", &p, e))?;
        while let Some(entry) = rd.next_entry().await.map_err(list_err)? {
            let name = entry.file_name().to_string_lossy().to_string();
            let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
            out.push(if is_dir { format!("{name}/") } else { name });
        }
        out.sort();
        Ok(out)
    }

    async fn stat(&self, path: &str) -> SeamResult<FileStat> {
        let p = self.resolve(path)?;
        let meta = tokio::fs::metadata(&p).await.map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                SeamError::NotFound(format!("{} not found", p.display()))
            }
            _ => SeamError::Io(format!("stat {}: {e}", p.display())),
        })?;
        let modified_ms = meta.modified().ok().and_then(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .ok()
                .map(|d| d.as_millis() as i64)
        });
        Ok(FileStat {
            path: p.to_string_lossy().to_string(),
            is_dir: meta.is_dir(),
            len: meta.len(),
            modified_ms,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::policy::PermissionPreset;

    #[test]
    fn next_entry_io_error_is_mapped() {
        assert!(matches!(
            list_err(std::io::Error::other("x")),
            SeamError::Io(_)
        ));
    }

    fn fixture() -> (tempfile::TempDir, LocalFs) {
        let dir = tempfile::tempdir().unwrap();
        let policy =
            Arc::new(SandboxPolicy::new(dir.path(), PermissionPreset::WorkspaceWrite).unwrap());
        let fs = LocalFs::new(policy);
        (dir, fs)
    }

    #[tokio::test]
    async fn write_read_list_stat_roundtrip() {
        let (_dir, fs) = fixture();
        fs.write("a/b.txt", "hello").await.unwrap();
        assert_eq!(fs.read("a/b.txt").await.unwrap(), "hello");
        let entries = fs.list("a").await.unwrap();
        assert_eq!(entries, vec!["b.txt".to_string()]);
        let stat = fs.stat("a/b.txt").await.unwrap();
        assert_eq!(stat.len, 5);
        assert!(!stat.is_dir);
        assert!(stat.modified_ms.is_some());
        // Directory listing marks directories with a trailing slash.
        fs.write("a/sub/x.txt", "x").await.unwrap();
        assert!(fs.list("a").await.unwrap().contains(&"sub/".to_string()));
    }

    #[tokio::test]
    async fn edit_replaces_once_and_reports_missing_text() {
        let (_dir, fs) = fixture();
        fs.write("f.txt", "a a a").await.unwrap();
        assert_eq!(fs.edit("f.txt", "a", "b").await.unwrap(), 1);
        assert_eq!(fs.read("f.txt").await.unwrap(), "b a a");
        assert!(matches!(
            fs.edit("f.txt", "zzz", "y").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(
            fs.edit("f.txt", "", "y").await,
            Err(SeamError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn escape_and_missing_paths_are_denied() {
        let (_dir, fs) = fixture();
        assert!(matches!(
            fs.read("../outside.txt").await,
            Err(SeamError::Denied(_))
        ));
        assert!(matches!(fs.read("nope.txt").await, Err(SeamError::Io(_))));
        assert!(matches!(
            fs.stat("nope.txt").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[test]
    fn policy_and_error_helpers_are_exposed() {
        let (dir, fs) = fixture();
        assert_eq!(fs.policy().root(), dir.path().canonicalize().unwrap());
        assert!(matches!(
            map_policy(PolicyError::NoRoot),
            SeamError::Unavailable(_)
        ));
        assert!(matches!(
            map_policy(PolicyError::Invalid("x".into())),
            SeamError::Invalid(_)
        ));
        assert!(matches!(
            io_err(
                "op",
                std::path::Path::new("/x"),
                std::io::Error::other("boom")
            ),
            SeamError::Io(_)
        ));
    }

    #[tokio::test]
    async fn io_failures_are_surfaced() {
        let (_dir, fs) = fixture();
        fs.write("a", "i am a file").await.unwrap();
        assert!(
            matches!(fs.write("a/child.txt", "x").await, Err(SeamError::Io(_))),
            "mkdir over a file must fail"
        );
        fs.write("dir/inside.txt", "x").await.unwrap();
        assert!(
            matches!(
                fs.write("dir", "overwrite a directory").await,
                Err(SeamError::Io(_))
            ),
            "writing onto a directory must fail"
        );
        fs.write("plain.txt", "body").await.unwrap();
        assert!(matches!(
            fs.edit("plain.txt", "zzz", "x").await,
            Err(SeamError::NotFound(_))
        ));
        assert!(
            matches!(fs.list("plain.txt").await, Err(SeamError::Io(_))),
            "listing a file must fail"
        );
        assert!(matches!(
            fs.read("plain.txt/child").await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(
            fs.edit("missing.txt", "a", "b").await,
            Err(SeamError::Io(_))
        ));

        #[cfg(unix)]
        {
            let loop_path = _dir.path().join("loop");
            std::os::unix::fs::symlink("loop", &loop_path).unwrap();
            assert!(
                matches!(fs.stat("loop").await, Err(SeamError::Io(_))),
                "a symlink loop is an IO error, not NotFound"
            );
        }
    }

    #[tokio::test]
    async fn symlink_escape_is_denied() {
        let (_dir, _fs) = fixture();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), "s").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(outside.path(), _dir.path().join("link")).unwrap();
            assert!(matches!(
                _fs.read("link/secret.txt").await,
                Err(SeamError::Denied(_))
            ));
        }
    }
}
