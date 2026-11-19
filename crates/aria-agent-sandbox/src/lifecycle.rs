//! Sandbox lifecycle state — **orthogonal to task state**.
//!
//! Runta exposes `agent.status` as "the runtime's lifecycle state", which makes
//! `status: "running"` look like "the initial task finished". Here the two
//! machines are separate types: [`SandboxLifecycle`] says what the sandbox is
//! doing, and the run/turn state lives in `AgentEvent` (queued → in progress →
//! done/failed). Nothing in this module may be used to answer "is the task done".
//!
//! Illegal transitions are errors, never silent coercions: a caller that tries to
//! pause a destroyed sandbox gets an actionable message instead of a success.

use crate::SandboxError;
use serde::{Deserialize, Serialize};

/// Wall-clock milliseconds since the epoch (0 if the clock is before 1970).
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the sandbox itself is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLifecycle {
    /// Container/VM is being created.
    Creating,
    /// Accepting `exec`.
    Running,
    /// Frozen in place: vCPU stops billing, memory is retained.
    Paused,
    /// Stopped with its filesystem retained: only disk bills.
    Suspended,
    /// Gone. Nothing bills.
    Destroyed,
    /// Creation or a transition failed; the sandbox is unusable.
    Failed,
}

impl SandboxLifecycle {
    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxLifecycle::Creating => "creating",
            SandboxLifecycle::Running => "running",
            SandboxLifecycle::Paused => "paused",
            SandboxLifecycle::Suspended => "suspended",
            SandboxLifecycle::Destroyed => "destroyed",
            SandboxLifecycle::Failed => "failed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "creating" => Some(SandboxLifecycle::Creating),
            "running" => Some(SandboxLifecycle::Running),
            "paused" => Some(SandboxLifecycle::Paused),
            "suspended" | "stopped" | "exited" => Some(SandboxLifecycle::Suspended),
            "destroyed" | "removed" => Some(SandboxLifecycle::Destroyed),
            "failed" | "dead" => Some(SandboxLifecycle::Failed),
            _ => None,
        }
    }

    /// Terminal states accept no further work.
    pub fn is_terminal(&self) -> bool {
        matches!(self, SandboxLifecycle::Destroyed)
    }

    /// Can this state accept `exec`?
    pub fn accepts_exec(&self) -> bool {
        matches!(self, SandboxLifecycle::Running)
    }

    /// Which state this may move to. Anything not listed is a bug in the caller.
    pub fn allowed_next(&self) -> &'static [SandboxLifecycle] {
        match self {
            SandboxLifecycle::Creating => &[
                SandboxLifecycle::Running,
                SandboxLifecycle::Failed,
                SandboxLifecycle::Destroyed,
            ],
            SandboxLifecycle::Running => &[
                SandboxLifecycle::Paused,
                SandboxLifecycle::Suspended,
                SandboxLifecycle::Destroyed,
                SandboxLifecycle::Failed,
            ],
            SandboxLifecycle::Paused => &[
                SandboxLifecycle::Running,
                SandboxLifecycle::Destroyed,
                SandboxLifecycle::Failed,
            ],
            SandboxLifecycle::Suspended => &[
                SandboxLifecycle::Running,
                SandboxLifecycle::Destroyed,
                SandboxLifecycle::Failed,
            ],
            SandboxLifecycle::Failed => &[SandboxLifecycle::Destroyed],
            SandboxLifecycle::Destroyed => &[],
        }
    }

    /// Validate a transition, returning the next state or an actionable error.
    pub fn transition_to(&self, next: SandboxLifecycle) -> Result<SandboxLifecycle, SandboxError> {
        if next == *self {
            return Ok(next);
        }
        if self.allowed_next().contains(&next) {
            return Ok(next);
        }
        Err(SandboxError::Exec(format!(
            "illegal sandbox lifecycle transition {} -> {} (allowed: {})",
            self.as_str(),
            next.as_str(),
            if self.allowed_next().is_empty() {
                "none".to_string()
            } else {
                self.allowed_next()
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join("|")
            }
        )))
    }
}

impl std::fmt::Display for SandboxLifecycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Capabilities a backend actually offers. Defaults are all `false` so a backend
/// that forgets to declare something fails closed at the call site rather than
/// returning a fake success.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct SandboxCapabilities {
    pub pause: bool,
    pub suspend: bool,
    pub checkpoint: bool,
    pub restore: bool,
    pub resize_memory: bool,
}

impl SandboxCapabilities {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn docker() -> Self {
        Self {
            pause: true,
            suspend: true,
            checkpoint: true,
            restore: true,
            resize_memory: false,
        }
    }

    pub fn all() -> Self {
        Self {
            pause: true,
            suspend: true,
            checkpoint: true,
            restore: true,
            resize_memory: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_happy_path_is_legal() {
        assert_eq!(
            SandboxLifecycle::Creating
                .transition_to(SandboxLifecycle::Running)
                .unwrap(),
            SandboxLifecycle::Running
        );
        assert_eq!(
            SandboxLifecycle::Running
                .transition_to(SandboxLifecycle::Paused)
                .unwrap(),
            SandboxLifecycle::Paused
        );
        assert_eq!(
            SandboxLifecycle::Paused
                .transition_to(SandboxLifecycle::Running)
                .unwrap(),
            SandboxLifecycle::Running
        );
        assert_eq!(
            SandboxLifecycle::Running
                .transition_to(SandboxLifecycle::Suspended)
                .unwrap(),
            SandboxLifecycle::Suspended
        );
        assert_eq!(
            SandboxLifecycle::Suspended
                .transition_to(SandboxLifecycle::Running)
                .unwrap(),
            SandboxLifecycle::Running
        );
        assert_eq!(
            SandboxLifecycle::Running
                .transition_to(SandboxLifecycle::Destroyed)
                .unwrap(),
            SandboxLifecycle::Destroyed
        );
    }

    #[test]
    fn a_transition_to_the_same_state_is_a_no_op() {
        for state in [
            SandboxLifecycle::Creating,
            SandboxLifecycle::Running,
            SandboxLifecycle::Paused,
            SandboxLifecycle::Suspended,
            SandboxLifecycle::Failed,
            SandboxLifecycle::Destroyed,
        ] {
            assert_eq!(state.transition_to(state).unwrap(), state);
        }
    }

    #[test]
    fn a_failed_container_can_only_be_destroyed() {
        assert_eq!(
            SandboxLifecycle::Failed
                .transition_to(SandboxLifecycle::Destroyed)
                .unwrap(),
            SandboxLifecycle::Destroyed
        );
        assert!(SandboxLifecycle::Failed
            .transition_to(SandboxLifecycle::Running)
            .is_err());
        assert!(now_ms() > 0);
    }

    #[test]
    fn illegal_transitions_are_refused_with_the_allowed_set() {
        let err = SandboxLifecycle::Destroyed
            .transition_to(SandboxLifecycle::Running)
            .unwrap_err();
        assert!(err.to_string().contains("illegal"), "{err}");
        assert!(err.to_string().contains("allowed: none"), "{err}");

        let err = SandboxLifecycle::Paused
            .transition_to(SandboxLifecycle::Suspended)
            .unwrap_err();
        assert!(err
            .to_string()
            .contains("allowed: running|destroyed|failed"));

        assert!(SandboxLifecycle::Creating
            .transition_to(SandboxLifecycle::Paused)
            .is_err());
    }

    #[test]
    fn terminal_and_exec_predicates() {
        assert!(SandboxLifecycle::Destroyed.is_terminal());
        assert!(!SandboxLifecycle::Failed.is_terminal());
        assert!(SandboxLifecycle::Running.accepts_exec());
        for state in [
            SandboxLifecycle::Creating,
            SandboxLifecycle::Paused,
            SandboxLifecycle::Suspended,
            SandboxLifecycle::Destroyed,
            SandboxLifecycle::Failed,
        ] {
            assert!(!state.accepts_exec(), "{state}");
        }
    }

    #[test]
    fn wire_names_round_trip() {
        for state in [
            SandboxLifecycle::Creating,
            SandboxLifecycle::Running,
            SandboxLifecycle::Paused,
            SandboxLifecycle::Suspended,
            SandboxLifecycle::Destroyed,
            SandboxLifecycle::Failed,
        ] {
            assert_eq!(SandboxLifecycle::parse(state.as_str()), Some(state));
            assert_eq!(state.to_string(), state.as_str());
        }
        assert_eq!(
            SandboxLifecycle::parse("stopped"),
            Some(SandboxLifecycle::Suspended)
        );
        assert_eq!(
            SandboxLifecycle::parse("removed"),
            Some(SandboxLifecycle::Destroyed)
        );
        assert_eq!(SandboxLifecycle::parse("sleepy"), None);
    }

    #[test]
    fn capabilities_default_to_nothing() {
        let none = SandboxCapabilities::none();
        assert!(!none.pause && !none.suspend && !none.checkpoint && !none.restore);
        assert!(SandboxCapabilities::all().resize_memory);
        assert!(SandboxCapabilities::docker().checkpoint);
        assert!(!SandboxCapabilities::docker().resize_memory);
        assert_eq!(SandboxCapabilities::default(), SandboxCapabilities::none());
    }
}
