//! Task lifecycle: the states a task may be in and the transitions that are
//! legal between them.
//!
//! The table is deliberately narrow. A live task may refresh its own state (the
//! same-state transition, used when a status message changes but the phase does
//! not), and a terminal task admits nothing at all — that is what makes
//! `cancel` and a late `completed` race-safe: whichever CAS lands first wins and
//! the loser is rejected instead of overwriting the outcome.

use serde::{Deserialize, Serialize};
use std::fmt;

/// One step of the A2A task lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TaskState {
    #[serde(rename = "submitted")]
    Submitted,
    #[serde(rename = "working")]
    Working,
    /// Parked on a human: an approval or a question is waiting for `resume`.
    #[serde(rename = "input-required", alias = "input_required")]
    InputRequired,
    #[serde(rename = "completed")]
    Completed,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "canceled", alias = "cancelled")]
    Canceled,
    /// A state we never write but must be able to read back.
    #[serde(rename = "unknown")]
    Unknown,
}

impl TaskState {
    /// The canonical wire/DB spelling (`input-required`, not `input_required`).
    pub fn as_str(self) -> &'static str {
        match self {
            TaskState::Submitted => "submitted",
            TaskState::Working => "working",
            TaskState::InputRequired => "input-required",
            TaskState::Completed => "completed",
            TaskState::Failed => "failed",
            TaskState::Canceled => "canceled",
            TaskState::Unknown => "unknown",
        }
    }

    /// Parse a stored or received state, accepting both the canonical spelling
    /// and the two aliases other implementations use.
    pub fn parse(raw: &str) -> Option<Self> {
        Some(match raw.trim() {
            "submitted" => TaskState::Submitted,
            "working" => TaskState::Working,
            "input-required" | "input_required" => TaskState::InputRequired,
            "completed" => TaskState::Completed,
            "failed" => TaskState::Failed,
            "canceled" | "cancelled" => TaskState::Canceled,
            "unknown" => TaskState::Unknown,
            _ => return None,
        })
    }

    /// Whether no further transition may leave this state.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TaskState::Completed | TaskState::Failed | TaskState::Canceled
        )
    }

    /// Whether `next` is a legal successor (or a refresh of this same state).
    pub fn can_transition(self, next: TaskState) -> bool {
        if self == next {
            return !self.is_terminal();
        }
        matches!(
            (self, next),
            (
                TaskState::Submitted,
                TaskState::Working | TaskState::Failed | TaskState::Canceled
            ) | (
                TaskState::Working,
                TaskState::InputRequired
                    | TaskState::Completed
                    | TaskState::Failed
                    | TaskState::Canceled
            ) | (
                TaskState::InputRequired,
                TaskState::Working | TaskState::Completed | TaskState::Failed | TaskState::Canceled
            )
        )
    }
}

impl fmt::Display for TaskState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
