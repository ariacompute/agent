//! Small pure helpers shared by the API layer (id generation, clocks).

use rand::RngCore;

/// `<prefix>_<24 hex chars>` — matches the OpenAI id shape closely enough for
/// clients that only rely on the prefix.
pub fn new_id(prefix: &str) -> String {
    let mut bytes = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut bytes);
    let raw: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}_{raw}")
}

/// Current wall clock in epoch milliseconds (the wire unit for `created_at`).
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_are_prefixed_and_unique() {
        let a = new_id("agent");
        let b = new_id("agent");
        assert!(a.starts_with("agent_"));
        assert_eq!(a.len(), "agent_".len() + 24);
        assert_ne!(a, b);
    }

    #[test]
    fn now_ms_is_a_plausible_epoch() {
        // After 2020-01-01 and before 2100-01-01: catches a unit mix-up
        // (seconds vs milliseconds) without depending on the wall clock.
        let now = now_ms();
        assert!(now > 1_577_836_800_000, "expected millis, got {now}");
        assert!(now < 4_102_444_800_000, "expected millis, got {now}");
    }
}
