//! Model seam: token accounting rides along with the reply.
//!
//! dsh keeps `ctx.llm` provider-agnostic: the agent loop and compaction call a
//! *stream service*, never a vendor SDK. [`TokenUsage`] is the one piece of
//! provider metadata the loop needs, and it is optional — backends that do not
//! report usage fall back to [`estimate_tokens`](crate::token_meter::estimate_tokens).

use serde::{Deserialize, Serialize};

/// Token accounting for one model call.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub model: String,
}

impl TokenUsage {
    pub fn new(input_tokens: u64, output_tokens: u64, model: impl Into<String>) -> Self {
        Self {
            input_tokens,
            output_tokens,
            model: model.into(),
        }
    }

    pub fn total(&self) -> u64 {
        self.input_tokens.saturating_add(self.output_tokens)
    }

    /// Merge another call's accounting into this one (per-turn totals).
    pub fn merge(&mut self, other: &TokenUsage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        if self.model.is_empty() {
            self.model = other.model.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn totals_and_merge_saturate() {
        let mut a = TokenUsage::new(10, 5, "m1");
        assert_eq!(a.total(), 15);
        a.merge(&TokenUsage::new(1, 2, "m2"));
        assert_eq!(a.input_tokens, 11);
        assert_eq!(a.output_tokens, 7);
        assert_eq!(a.model, "m1");
        a.input_tokens = u64::MAX;
        a.merge(&TokenUsage::new(1, 0, ""));
        assert_eq!(a.input_tokens, u64::MAX);
    }

    #[test]
    fn empty_model_is_filled_from_merge() {
        let mut a = TokenUsage::default();
        a.merge(&TokenUsage::new(1, 1, "gpt"));
        assert_eq!(a.model, "gpt");
    }

    #[test]
    fn serde_defaults_missing_fields() {
        let u: TokenUsage = serde_json::from_str(r#"{"input_tokens":3}"#).unwrap();
        assert_eq!(u.input_tokens, 3);
        assert_eq!(u.output_tokens, 0);
        assert_eq!(u.model, "");
    }
}
