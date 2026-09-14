//! Unified error type for the `agent-reef` self-improvement loop.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReefError {
    #[error("storage error: {0}")]
    Storage(String),
    #[error("serialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("engine error: {0}")]
    Engine(String),
    #[error("git error: {0}")]
    Git(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("no eligible records for evolution")]
    NoEligible,
    #[error("config error: {0}")]
    Config(String),
}
