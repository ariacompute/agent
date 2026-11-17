//! Compaction policy.
//!
//! dsh consumes two signals: a **post-step pressure** event and a **request
//! error recovery** event. There is intentionally **no model-facing compaction
//! tool** — compaction is an engine decision (ADR-0015).
//!
//! [`CompactionPolicy`] turns those two signals into a decision, keeping the
//! policy pure so it is trivially testable.

use serde::{Deserialize, Serialize};

/// Why compaction was triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactReason {
    /// Context pressure crossed the threshold.
    Pressure,
    /// The provider rejected the request as too long.
    ContextOverflow,
    /// An explicit operator request.
    Manual,
}

impl CompactReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompactReason::Pressure => "pressure",
            CompactReason::ContextOverflow => "context_overflow",
            CompactReason::Manual => "manual",
        }
    }
}

/// Decision for one compaction opportunity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactDecision {
    /// Nothing to do.
    Skip,
    /// Compact now, with this reason.
    Compact(CompactReason),
}

/// Thresholds for compaction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionPolicy {
    /// Fraction of the context limit that triggers pressure compaction (0..=1).
    pub pressure_ratio: f64,
    /// Retry once after an overflow error before giving up.
    pub retry_on_overflow: bool,
}

impl Default for CompactionPolicy {
    fn default() -> Self {
        Self {
            pressure_ratio: crate::token_meter::DEFAULT_PRESSURE_RATIO,
            retry_on_overflow: true,
        }
    }
}

impl CompactionPolicy {
    pub fn new(pressure_ratio: f64, retry_on_overflow: bool) -> Self {
        Self {
            pressure_ratio: if pressure_ratio > 0.0 && pressure_ratio <= 1.0 {
                pressure_ratio
            } else {
                crate::token_meter::DEFAULT_PRESSURE_RATIO
            },
            retry_on_overflow,
        }
    }

    /// Decide from the post-step usage signal.
    pub fn after_step(&self, used_tokens: u64, limit_tokens: u64) -> CompactDecision {
        if limit_tokens == 0 {
            return CompactDecision::Skip;
        }
        let threshold = (limit_tokens as f64 * self.pressure_ratio) as u64;
        if used_tokens >= threshold {
            CompactDecision::Compact(CompactReason::Pressure)
        } else {
            CompactDecision::Skip
        }
    }

    /// Decide from a provider error message (`None` = no error).
    pub fn after_error(&self, error: Option<&str>) -> CompactDecision {
        if !self.retry_on_overflow {
            return CompactDecision::Skip;
        }
        match error {
            Some(message) if looks_like_overflow(message) => {
                CompactDecision::Compact(CompactReason::ContextOverflow)
            }
            _ => CompactDecision::Skip,
        }
    }
}

/// Recognise the provider-agnostic phrasings for "context too long".
pub fn looks_like_overflow(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    [
        "context length",
        "context_length_exceeded",
        "maximum context",
        "too many tokens",
        "context window",
        "prompt is too long",
    ]
    .iter()
    .any(|needle| m.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_decision_uses_the_ratio() {
        let p = CompactionPolicy::new(0.5, true);
        assert_eq!(p.after_step(49, 100), CompactDecision::Skip);
        assert_eq!(
            p.after_step(50, 100),
            CompactDecision::Compact(CompactReason::Pressure)
        );
        assert_eq!(p.after_step(1000, 0), CompactDecision::Skip);
    }

    #[test]
    fn invalid_ratio_falls_back() {
        assert_eq!(
            CompactionPolicy::new(0.0, true).pressure_ratio,
            crate::token_meter::DEFAULT_PRESSURE_RATIO
        );
        assert_eq!(
            CompactionPolicy::new(1.5, true).pressure_ratio,
            crate::token_meter::DEFAULT_PRESSURE_RATIO
        );
    }

    #[test]
    fn overflow_detection_matches_provider_phrasings() {
        assert!(looks_like_overflow(
            "This model's maximum context length is 8192"
        ));
        assert!(looks_like_overflow("error: context_length_exceeded"));
        assert!(looks_like_overflow("The prompt is too long"));
        assert!(!looks_like_overflow("rate limited"));
        let p = CompactionPolicy::default();
        assert_eq!(
            p.after_error(Some("maximum context length exceeded")),
            CompactDecision::Compact(CompactReason::ContextOverflow)
        );
        assert_eq!(p.after_error(None), CompactDecision::Skip);
        assert_eq!(p.after_error(Some("boom")), CompactDecision::Skip);
        let no_retry = CompactionPolicy::new(0.75, false);
        assert_eq!(
            no_retry.after_error(Some("context length exceeded")),
            CompactDecision::Skip
        );
    }

    #[test]
    fn reasons_serialize_as_snake_case() {
        for r in [
            CompactReason::Pressure,
            CompactReason::ContextOverflow,
            CompactReason::Manual,
        ] {
            let v = serde_json::to_value(r).unwrap();
            assert_eq!(v, serde_json::json!(r.as_str()));
        }
    }
}
