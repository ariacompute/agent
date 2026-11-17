//! Token accounting + context pressure.
//!
//! dsh's `token-meter` subsystem feeds two decisions: *compact now* (pressure)
//! and *compact and retry* (provider reported a context overflow). Usage reported
//! by the provider wins; when a backend reports nothing we estimate locally so
//! the pressure signal still exists.

use crate::seams::TokenUsage;

/// Bytes per token used by the fallback estimator (conservative for English,
/// generous for CJK — over-estimating is the safe direction for compaction).
pub const BYTES_PER_TOKEN: usize = 4;

/// Estimate token count from text. Always ≥ 1 for non-empty input so a stream of
/// one-word turns still accumulates.
pub fn estimate_tokens(text: &str) -> u64 {
    if text.is_empty() {
        return 0;
    }
    let chars = text.chars().count() as u64;
    chars.div_ceil(BYTES_PER_TOKEN as u64).max(1)
}

/// Default context limit assumed when the deployment does not configure one.
pub const DEFAULT_CONTEXT_LIMIT: u64 = 128_000;
/// Compact once this fraction of the limit is consumed.
pub const DEFAULT_PRESSURE_RATIO: f64 = 0.75;

/// Running meter for one session.
#[derive(Debug, Clone)]
pub struct TokenMeter {
    limit: u64,
    pressure_ratio: f64,
    total: TokenUsage,
    calls: u64,
}

impl Default for TokenMeter {
    fn default() -> Self {
        Self::new(DEFAULT_CONTEXT_LIMIT, DEFAULT_PRESSURE_RATIO)
    }
}

impl TokenMeter {
    pub fn new(limit: u64, pressure_ratio: f64) -> Self {
        Self {
            limit,
            pressure_ratio: if pressure_ratio > 0.0 && pressure_ratio <= 1.0 {
                pressure_ratio
            } else {
                DEFAULT_PRESSURE_RATIO
            },
            total: TokenUsage::default(),
            calls: 0,
        }
    }

    /// Record provider-reported usage.
    pub fn observe(&mut self, usage: &TokenUsage) {
        self.total.merge(usage);
        self.calls += 1;
    }

    /// Record a call whose provider reported nothing, using the local estimate.
    pub fn observe_text(&mut self, input: &str, output: &str, model: &str) {
        self.observe(&TokenUsage::new(
            estimate_tokens(input),
            estimate_tokens(output),
            model,
        ));
    }

    pub fn used(&self) -> u64 {
        self.total.total()
    }

    pub fn calls(&self) -> u64 {
        self.calls
    }

    pub fn limit(&self) -> u64 {
        self.limit
    }

    pub fn usage(&self) -> &TokenUsage {
        &self.total
    }

    /// Effective compaction threshold ratio (invalid input falls back).
    pub fn pressure_ratio(&self) -> f64 {
        self.pressure_ratio
    }

    /// Whether the context is fat enough that compaction should run *before* the
    /// next model call.
    pub fn under_pressure(&self) -> bool {
        if self.limit == 0 {
            return false;
        }
        let threshold = (self.limit as f64 * self.pressure_ratio) as u64;
        self.used() >= threshold
    }

    /// Called after a compaction so pressure is re-evaluated from scratch.
    pub fn reset(&mut self) {
        self.total = TokenUsage::default();
        self.calls = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimator_is_monotonic_and_char_based() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcd"), 1);
        assert_eq!(estimate_tokens("abcde"), 2);
        assert!(estimate_tokens("日本語") >= 1);
        assert!(estimate_tokens(&"x".repeat(400)) > estimate_tokens(&"x".repeat(40)));
    }

    #[test]
    fn records_reported_and_estimated_usage() {
        let mut m = TokenMeter::new(1000, 0.5);
        m.observe(&TokenUsage::new(100, 20, "gpt"));
        assert_eq!(m.used(), 120);
        assert_eq!(m.calls(), 1);
        assert!(!m.under_pressure());
        m.observe_text("abcd", "efgh", "gpt");
        assert_eq!(m.calls(), 2);
        assert_eq!(m.used(), 122);
        assert_eq!(m.usage().model, "gpt");
        assert_eq!(m.limit(), 1000);
    }

    #[test]
    fn pressure_threshold_triggers() {
        let mut m = TokenMeter::new(100, 0.5);
        m.observe(&TokenUsage::new(49, 0, "m"));
        assert!(!m.under_pressure());
        m.observe(&TokenUsage::new(1, 0, "m"));
        assert!(m.under_pressure());
        m.reset();
        assert_eq!(m.used(), 0);
        assert!(!m.under_pressure());
    }

    #[test]
    fn default_meter_uses_the_documented_limits() {
        let m = TokenMeter::default();
        assert!(!m.under_pressure());
        assert_eq!(m.pressure_ratio(), DEFAULT_PRESSURE_RATIO);
    }

    #[test]
    fn invalid_configuration_falls_back() {
        let m = TokenMeter::new(0, 0.0);
        assert!(!m.under_pressure());
        assert_eq!(m.pressure_ratio(), DEFAULT_PRESSURE_RATIO);
        let m = TokenMeter::new(10, 2.0);
        assert_eq!(m.pressure_ratio(), DEFAULT_PRESSURE_RATIO);
    }
}
