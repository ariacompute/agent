//! Human-in-the-loop questions seam (`ctx.userQuestions`).
//!
//! `tool-ask-user` pauses on a provider-agnostic `ask()` future. With no provider
//! the call fails closed: the model gets an actionable error instead of an empty
//! string that looks like a user answer (ADR-0016).

use super::{SeamError, SeamResult};
use async_trait::async_trait;

#[async_trait]
pub trait UserQuestions: Send + Sync {
    /// Ask `question`, optionally offering `options` (empty = free-form).
    async fn ask(&self, question: &str, options: &[String]) -> SeamResult<String>;
}

/// Default provider: no interactive channel.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoQuestions;

#[async_trait]
impl UserQuestions for NoQuestions {
    async fn ask(&self, _question: &str, _options: &[String]) -> SeamResult<String> {
        Err(SeamError::Unavailable(
            "no user-question provider is configured (headless run)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_questions_fails_closed() {
        let q = NoQuestions;
        let err = q
            .ask("which env?", &["dev".into(), "prod".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SeamError::Unavailable(_)));
        assert_eq!(err.code(), "unavailable");
    }
}
