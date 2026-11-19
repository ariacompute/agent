//! Checkpoint references and restore semantics.
//!
//! A checkpoint is a **filesystem + process-state image** produced by committing
//! the running container. Restoring always derives a **new** sandbox: the source
//! is never rolled back in place, so a checkpoint is repeatable and a failed
//! restore cannot damage the thing it came from (ADR-0022).
//!
//! Restores go through the same quota gate as `spawn` — that gate lives in
//! `agent-governance` (`QuotaGuardSandbox`) so no backend can forget it.

use serde::{Deserialize, Serialize};

/// Stable handle to one committed image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckpointRef {
    pub id: String,
    /// Image reference the backend can start a new sandbox from.
    pub image: String,
    /// Sandbox the checkpoint was taken from.
    pub source: String,
    pub created_at_ms: u64,
}

impl CheckpointRef {
    pub fn new(image: impl Into<String>, source: impl Into<String>, created_at_ms: u64) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            image: image.into(),
            source: source.into(),
            created_at_ms,
        }
    }

    /// Validate the shape before it is handed to a backend.
    pub fn validate(&self) -> Result<(), crate::SandboxError> {
        if self.image.trim().is_empty() {
            return Err(crate::SandboxError::Exec(
                "checkpoint image reference must not be empty".into(),
            ));
        }
        if self.source.trim().is_empty() {
            return Err(crate::SandboxError::Exec(
                "checkpoint source sandbox must not be empty".into(),
            ));
        }
        Ok(())
    }
}

/// Image repository name for a checkpoint of `source`.
pub fn checkpoint_repo(source: &str) -> String {
    let sanitized: String = source
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    format!("aria-ckpt-{sanitized}")
}

/// Image reference for a committed checkpoint.
///
/// Docker normally answers the commit with the new image id; the repository name
/// is the fallback when it does not (older engines, partial responses). The
/// fallback is also what real engines hit today: they return `{"Id": …}` while
/// bollard reads `{"ID": …}`, and `repo:tag` is the reference we just created.
pub(crate) fn checkpoint_image(id: Option<&str>, repo: &str) -> String {
    match id {
        Some(id) if !id.trim().is_empty() => id.to_string(),
        _ => format!("{repo}:latest"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SandboxError;

    #[test]
    fn refs_carry_a_unique_id_and_validate_their_shape() {
        let a = CheckpointRef::new("aria-ckpt-x:latest", "sandbox-1", 1_000);
        let b = CheckpointRef::new("aria-ckpt-x:latest", "sandbox-1", 1_000);
        assert_ne!(a.id, b.id, "ids must be unique even for identical inputs");
        assert_eq!(a.created_at_ms, 1_000);
        assert!(a.validate().is_ok());

        let mut broken = a.clone();
        broken.image = "  ".into();
        assert!(matches!(broken.validate(), Err(SandboxError::Exec(_))));
        broken.image = "img".into();
        broken.source = String::new();
        assert!(broken.validate().is_err());
    }

    #[test]
    fn round_trips_through_json() {
        let reference = CheckpointRef::new("img", "sbx", 7);
        let json = serde_json::to_string(&reference).unwrap();
        let back: CheckpointRef = serde_json::from_str(&json).unwrap();
        assert_eq!(back, reference);
    }

    #[test]
    fn a_committed_image_uses_the_id_or_falls_back_to_the_repo() {
        assert_eq!(checkpoint_image(Some("sha256:abc"), "r"), "sha256:abc");
        assert_eq!(checkpoint_image(Some("  "), "r"), "r:latest");
        assert_eq!(checkpoint_image(None, "r"), "r:latest");
    }

    #[test]
    fn repo_names_are_docker_safe() {
        assert_eq!(
            checkpoint_repo("aria-sandbox-ABC"),
            "aria-ckpt-aria-sandbox-abc"
        );
        assert_eq!(checkpoint_repo("a/b:c d"), "aria-ckpt-a-b-c-d");
        assert!(checkpoint_repo("x y").chars().all(|c| c != ' '));
    }
}
