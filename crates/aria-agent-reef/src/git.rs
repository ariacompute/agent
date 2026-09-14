//! Git versioning of harness artifacts — the "Commit" half of the Reef loop.
//!
//! We shell out to the system `git` (no heavy `git2` dependency) to keep a
//! readable, roll-backable history of harness versions under `.reef/`. Every
//! committed winning harness is tagged `reef@<n>`. All operations are
//! fail-closed: if `git` is unavailable the caller keeps serving the current
//! harness rather than crashing.

use crate::error::ReefError;
use std::path::Path;
use std::process::Command;

/// The tag prefix for versioned harness commits.
pub const TAG_PREFIX: &str = "reef@";

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn run_git(dir: &Path, args: &[&str]) -> Result<String, ReefError> {
    if !git_available() {
        return Err(ReefError::Git("git binary not found on PATH".into()));
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .output()
        .map_err(|e| ReefError::Git(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr).to_string();
        return Err(ReefError::Git(format!(
            "git {} failed: {}",
            args.join(" "),
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Initialize a git repo at `dir` (idempotent — safe to call repeatedly).
pub fn init_repo(dir: &Path) -> Result<(), ReefError> {
    std::fs::create_dir_all(dir).map_err(|e| ReefError::Storage(e.to_string()))?;
    // `git init` succeeds whether or not the repo already exists.
    run_git(dir, &["init"]).map(|_| ())
}

/// Stage everything and create a commit with the given message.
///
/// A local commit identity is supplied inline so commits succeed even when the
/// machine has no global `user.name`/`user.email` configured.
pub fn commit(dir: &Path, message: &str) -> Result<(), ReefError> {
    run_git(dir, &["add", "-A"])?;
    // If the working tree is clean there is nothing to commit — not an error.
    let status = run_git(dir, &["status", "--porcelain"])?;
    if status.trim().is_empty() {
        return Ok(());
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args([
            "-c",
            "user.email=reef@localhost",
            "-c",
            "user.name=reef",
            "commit",
            "-m",
            message,
        ])
        .output()
        .map_err(|e| ReefError::Git(e.to_string()))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(ReefError::Git(format!(
            "git commit failed: {}",
            stderr.trim()
        )));
    }
    Ok(())
}

/// Tag the current commit (e.g. `reef@3`).
pub fn tag(dir: &Path, tag: &str) -> Result<(), ReefError> {
    run_git(dir, &["tag", tag]).map(|_| ())
}

/// List all tags (unsorted raw output of `git tag`).
pub fn list_versions(dir: &Path) -> Result<Vec<String>, ReefError> {
    let out = run_git(dir, &["tag"])?;
    Ok(out
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

/// Parse the highest `reef@<n>` version number, or `0` if none exist.
pub fn current_version(dir: &Path) -> Result<usize, ReefError> {
    let mut max = 0usize;
    for t in list_versions(dir)? {
        if let Some(num) = t.strip_prefix(TAG_PREFIX) {
            if let Ok(n) = num.parse::<usize>() {
                max = max.max(n);
            }
        }
    }
    Ok(max)
}

/// Next version number (`current_version + 1`).
pub fn next_version(dir: &Path) -> Result<usize, ReefError> {
    Ok(current_version(dir)? + 1)
}

/// Full commit+tag for a winning harness: returns the new version number.
pub fn publish(dir: &Path, message: &str) -> Result<usize, ReefError> {
    let v = next_version(dir)?;
    commit(dir, &format!("{} (v{})", message, v))?;
    let tag_name = format!("{}{}", TAG_PREFIX, v);
    tag(dir, &tag_name)?;
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_git_is_git_error() {
        // We can't guarantee git is absent, but if it is, operations fail closed
        // with ReefError::Git. If git IS present we exercise the full
        // init/commit/tag flow (a real harness file is written first, matching
        // the engine's actual usage so a HEAD exists to tag).
        let dir = std::env::temp_dir().join(format!("reef_git_{}", uuid::Uuid::new_v4()));
        // Ensure a clean temp dir.
        let _ = std::fs::remove_dir_all(&dir);
        if git_available() {
            init_repo(&dir).unwrap();
            // A committed harness artifact is required before we can tag HEAD.
            std::fs::write(dir.join("system_prompt.md"), "You are agent.")
                .map_err(|e| ReefError::Storage(e.to_string()))
                .unwrap();
            commit(&dir, "baseline").unwrap();
            let v = publish(&dir, "winner").unwrap();
            assert_eq!(v, 1);
            let versions = list_versions(&dir).unwrap();
            assert!(versions.contains(&"reef@1".to_string()));
            assert_eq!(current_version(&dir).unwrap(), 1);
            assert_eq!(next_version(&dir).unwrap(), 2);
        } else {
            // No git: operations fail closed with ReefError::Git.
            assert!(matches!(init_repo(&dir), Err(ReefError::Git(_))));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn init_is_idempotent() {
        let dir = std::env::temp_dir().join(format!("reef_git_idem_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        if git_available() {
            init_repo(&dir).unwrap();
            init_repo(&dir).unwrap(); // second call must not error
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
