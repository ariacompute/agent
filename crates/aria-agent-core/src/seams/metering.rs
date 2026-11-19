//! Metering seam: resource usage accounted **by lifecycle state**.
//!
//! The point of the state split is that idle work costs less, and that is
//! provable rather than asserted (ADR-0022):
//!
//! | state      | vCPU | memory | disk |
//! |------------|------|--------|------|
//! | running    |  ✓   |   ✓    |  ✓   |
//! | paused     |  ✗   |   ✓    |  ✓   |
//! | suspended  |  ✗   |   ✗    |  ✓   |
//! | destroyed  |  ✗   |   ✗    |  ✗   |
//!
//! Estimates are **not** billable. [`EstimatedTokens`] is a distinct type so the
//! `ceil(chars/4)` heuristic used for context pressure cannot be passed to a
//! metering path by accident (Runta's X-Ray makes exactly that mistake).

use super::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Sandbox lifecycle state as far as accounting is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MeterState {
    Running,
    Paused,
    Suspended,
    Destroyed,
}

impl MeterState {
    pub fn as_str(&self) -> &'static str {
        match self {
            MeterState::Running => "running",
            MeterState::Paused => "paused",
            MeterState::Suspended => "suspended",
            MeterState::Destroyed => "destroyed",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "running" => Some(MeterState::Running),
            "paused" => Some(MeterState::Paused),
            "suspended" | "suspended_and_shut_down" => Some(MeterState::Suspended),
            "destroyed" | "deleted" => Some(MeterState::Destroyed),
            _ => None,
        }
    }

    /// Which dimensions are billable in this state.
    pub fn bills_cpu(&self) -> bool {
        matches!(self, MeterState::Running)
    }

    pub fn bills_memory(&self) -> bool {
        matches!(self, MeterState::Running | MeterState::Paused)
    }

    pub fn bills_disk(&self) -> bool {
        !matches!(self, MeterState::Destroyed)
    }
}

/// Integer-only usage so no rounding drift accumulates across many records.
/// `*_millis` are "unit × milliseconds" (e.g. `vcpu_millis` = vCPU·ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub vcpu_millis: u64,
    pub memory_mib_millis: u64,
    pub disk_mib_millis: u64,
}

impl ResourceUsage {
    /// Usage for `duration_ms` spent in `state`, given the reserved shape.
    pub fn for_state(
        state: MeterState,
        vcpus_x1000: u64,
        memory_mib: u64,
        disk_mib: u64,
        duration_ms: u64,
    ) -> Self {
        // vcpus_x1000 keeps fractional vCPU (e.g. 1500 = 1.5) integral.
        let cpu = if state.bills_cpu() {
            vcpus_x1000.saturating_mul(duration_ms) / 1000
        } else {
            0
        };
        let memory = if state.bills_memory() {
            memory_mib.saturating_mul(duration_ms)
        } else {
            0
        };
        let disk = if state.bills_disk() {
            disk_mib.saturating_mul(duration_ms)
        } else {
            0
        };
        Self {
            vcpu_millis: cpu,
            memory_mib_millis: memory,
            disk_mib_millis: disk,
        }
    }

    pub fn merge(&mut self, other: &ResourceUsage) {
        self.vcpu_millis = self.vcpu_millis.saturating_add(other.vcpu_millis);
        self.memory_mib_millis = self
            .memory_mib_millis
            .saturating_add(other.memory_mib_millis);
        self.disk_mib_millis = self.disk_mib_millis.saturating_add(other.disk_mib_millis);
    }

    pub fn is_zero(&self) -> bool {
        self.vcpu_millis == 0 && self.memory_mib_millis == 0 && self.disk_mib_millis == 0
    }
}

/// A model token estimate. Exists to make "do not bill this" a type error rather
/// than a code-review note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct EstimatedTokens(u64);

impl EstimatedTokens {
    pub fn from_text(text: &str) -> Self {
        Self(crate::token_meter::estimate_tokens(text))
    }

    pub fn value(&self) -> u64 {
        self.0
    }
}

/// Authentic provider-reported usage. Only this is billable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BilledTokens {
    pub input: u64,
    pub output: u64,
}

impl BilledTokens {
    pub fn new(input: u64, output: u64) -> Self {
        Self { input, output }
    }

    pub fn total(&self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// One closed accounting interval for one sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageRecord {
    pub id: String,
    pub principal_id: String,
    pub sandbox_id: String,
    pub state: MeterState,
    pub started_at_ms: u64,
    pub ended_at_ms: u64,
    pub resources: ResourceUsage,
    /// Provider-reported tokens, when a model call happened in this interval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<BilledTokens>,
}

impl UsageRecord {
    pub fn new(
        principal_id: impl Into<String>,
        sandbox_id: impl Into<String>,
        state: MeterState,
        started_at_ms: u64,
        ended_at_ms: u64,
        resources: ResourceUsage,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            principal_id: principal_id.into(),
            sandbox_id: sandbox_id.into(),
            state,
            started_at_ms,
            ended_at_ms,
            resources,
            tokens: None,
        }
    }

    pub fn duration_ms(&self) -> u64 {
        self.ended_at_ms.saturating_sub(self.started_at_ms)
    }
}

/// Aggregated usage for one principal.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageSummary {
    pub principal_id: String,
    pub records: u64,
    pub resources: ResourceUsage,
    #[serde(default)]
    pub tokens: BilledTokens,
    /// Rated spend, present only when the caller asked for pricing and a
    /// [`crate::seams::metering`] price book is configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priced_micros_usd: Option<u64>,
}

/// One reserved shape used for quota arithmetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub vcpus_x1000: u64,
    pub memory_mib: u64,
    pub disk_mib: u64,
}

impl Default for ResourceRequest {
    fn default() -> Self {
        Self {
            vcpus_x1000: 1000,
            memory_mib: 512,
            disk_mib: 1024,
        }
    }
}

impl ResourceRequest {
    /// Aggregate weight used by a simple sum-based quota. Keeps the accounting
    /// policy in one place so `spawn` and `restore` cannot diverge.
    pub fn weight(&self) -> u64 {
        self.vcpus_x1000
            .saturating_add(self.memory_mib)
            .saturating_add(self.disk_mib / 10)
    }
}

/// The seam.
#[async_trait]
pub trait MeteringSeam: Send + Sync {
    /// Append one closed interval.
    async fn record(&self, record: UsageRecord) -> SeamResult<()>;

    /// Aggregate for one principal.
    async fn summary(&self, principal_id: &str) -> SeamResult<UsageSummary>;

    /// Reserve capacity for a **new or restored** sandbox. Implementations must
    /// use the same function for both paths, so a checkpoint restore cannot
    /// bypass the quota that `spawn` enforces.
    async fn reserve(
        &self,
        principal_id: &str,
        sandbox_id: &str,
        request: ResourceRequest,
    ) -> SeamResult<()> {
        let _ = (principal_id, sandbox_id, request);
        Ok(())
    }

    /// Release a reservation (destroy/restore-failure rollback).
    async fn release(&self, principal_id: &str, sandbox_id: &str) -> SeamResult<()> {
        let _ = (principal_id, sandbox_id);
        Ok(())
    }
}

/// No accounting configured: records are accepted and discarded, summaries are
/// empty, reservations are unlimited. A deployment that wants a hard limit must
/// configure a ledger — silence here is a deliberate "metering is off", and the
/// governance stack logs that at startup.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMetering;

#[async_trait]
impl MeteringSeam for NoMetering {
    async fn record(&self, _record: UsageRecord) -> SeamResult<()> {
        Ok(())
    }

    async fn summary(&self, principal_id: &str) -> SeamResult<UsageSummary> {
        Ok(UsageSummary {
            principal_id: principal_id.to_string(),
            ..UsageSummary::default()
        })
    }
}

/// A metering seam that refuses everything — used to prove a deployment did not
/// configure accounting when it claimed to.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMeteringConfigured;

#[async_trait]
impl MeteringSeam for NoMeteringConfigured {
    async fn record(&self, _record: UsageRecord) -> SeamResult<()> {
        Err(SeamError::Unavailable(
            "no metering backend is configured for this deployment".into(),
        ))
    }

    async fn summary(&self, _principal_id: &str) -> SeamResult<UsageSummary> {
        Err(SeamError::Unavailable(
            "no metering backend is configured for this deployment".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_to_billing_dimension_mapping_is_the_contract() {
        assert!(MeterState::Running.bills_cpu());
        assert!(MeterState::Running.bills_memory());
        assert!(MeterState::Running.bills_disk());

        assert!(!MeterState::Paused.bills_cpu(), "pause stops vCPU billing");
        assert!(MeterState::Paused.bills_memory());
        assert!(MeterState::Paused.bills_disk());

        assert!(!MeterState::Suspended.bills_cpu());
        assert!(!MeterState::Suspended.bills_memory());
        assert!(MeterState::Suspended.bills_disk(), "disk is retained");

        assert!(!MeterState::Destroyed.bills_disk());
        assert_eq!(MeterState::Paused.as_str(), "paused");
        for (state, name) in [
            (MeterState::Running, "running"),
            (MeterState::Suspended, "suspended"),
            (MeterState::Destroyed, "destroyed"),
        ] {
            assert_eq!(state.as_str(), name);
            assert_eq!(MeterState::parse(name), Some(state));
        }
        assert_eq!(
            MeterState::parse("SUSPENDED_AND_SHUT_DOWN"),
            Some(MeterState::Suspended)
        );
        assert_eq!(MeterState::parse("paused"), Some(MeterState::Paused));
        assert_eq!(MeterState::parse("deleted"), Some(MeterState::Destroyed));
        assert_eq!(MeterState::parse("weird"), None);
    }

    #[test]
    fn states_parse_without_silent_fallbacks() {
        assert_eq!(MeterState::parse("RUNNING"), Some(MeterState::Running));
        assert_eq!(MeterState::parse("deleted"), Some(MeterState::Destroyed));
        assert_eq!(
            MeterState::parse("suspended_and_shut_down"),
            Some(MeterState::Suspended)
        );
        assert_eq!(MeterState::parse("sleeping"), None);
    }

    #[test]
    fn usage_only_counts_billable_dimensions() {
        let running = ResourceUsage::for_state(MeterState::Running, 2000, 1024, 4096, 3_600_000);
        assert_eq!(running.vcpu_millis, 2000 * 3_600_000 / 1000);
        assert_eq!(running.memory_mib_millis, 1024 * 3_600_000);
        assert_eq!(running.disk_mib_millis, 4096 * 3_600_000);

        let paused = ResourceUsage::for_state(MeterState::Paused, 2000, 1024, 4096, 3_600_000);
        assert_eq!(paused.vcpu_millis, 0);
        assert_eq!(paused.memory_mib_millis, 1024 * 3_600_000);

        let suspended = ResourceUsage::for_state(MeterState::Suspended, 2000, 1024, 4096, 1000);
        assert_eq!(suspended.vcpu_millis, 0);
        assert_eq!(suspended.memory_mib_millis, 0);
        assert_eq!(suspended.disk_mib_millis, 4096 * 1000);

        let destroyed = ResourceUsage::for_state(MeterState::Destroyed, 2000, 1024, 4096, 1000);
        assert!(destroyed.is_zero());

        let zero = ResourceUsage::for_state(MeterState::Running, 0, 0, 0, 5);
        assert!(zero.is_zero());
    }

    #[test]
    fn usage_merges_saturating() {
        let mut a = ResourceUsage {
            vcpu_millis: 1,
            memory_mib_millis: 2,
            disk_mib_millis: 3,
        };
        a.merge(&ResourceUsage {
            vcpu_millis: 10,
            memory_mib_millis: 20,
            disk_mib_millis: 30,
        });
        assert_eq!(
            (a.vcpu_millis, a.memory_mib_millis, a.disk_mib_millis),
            (11, 22, 33)
        );

        let mut maxed = ResourceUsage {
            vcpu_millis: u64::MAX,
            ..ResourceUsage::default()
        };
        maxed.merge(&ResourceUsage {
            vcpu_millis: 5,
            ..ResourceUsage::default()
        });
        assert_eq!(maxed.vcpu_millis, u64::MAX);
    }

    #[test]
    fn estimates_and_billed_tokens_are_different_types() {
        let estimated = EstimatedTokens::from_text("hello world");
        assert_eq!(estimated.value(), 3, "ceil(11/4)");
        assert_eq!(EstimatedTokens::from_text("").value(), 0);
        let billed = BilledTokens::new(10, 5);
        assert_eq!(billed.total(), 15);
    }

    #[test]
    fn records_report_duration_and_serialise() {
        let record = UsageRecord::new(
            "p1",
            "sbx",
            MeterState::Running,
            1_000,
            4_000,
            ResourceUsage::for_state(MeterState::Running, 1000, 512, 0, 3_000),
        );
        assert_eq!(record.duration_ms(), 3_000);
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.contains("\"state\":\"running\""));
        assert!(!json.contains("tokens"), "absent tokens stay absent");
        let back: UsageRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, record);

        let backwards = UsageRecord::new(
            "p",
            "s",
            MeterState::Running,
            10,
            1,
            ResourceUsage::default(),
        );
        assert_eq!(backwards.duration_ms(), 0, "never negative");
    }

    #[test]
    fn quota_weight_is_stable() {
        let req = ResourceRequest {
            vcpus_x1000: 2000,
            memory_mib: 1024,
            disk_mib: 10240,
        };
        assert_eq!(req.weight(), 2000 + 1024 + 1024);
        assert_eq!(ResourceRequest::default().weight(), 1000 + 512 + 102);
    }

    #[tokio::test]
    async fn default_seams_are_inert_but_not_erroring() {
        let metering = NoMetering;
        let record = UsageRecord::new(
            "p",
            "s",
            MeterState::Running,
            0,
            1,
            ResourceUsage::default(),
        );
        metering.record(record).await.unwrap();
        metering
            .reserve("p", "s", ResourceRequest::default())
            .await
            .unwrap();
        metering.release("p", "s").await.unwrap();
        let summary = metering.summary("p").await.unwrap();
        assert_eq!(summary.principal_id, "p");
        assert_eq!(summary.records, 0);
    }

    #[tokio::test]
    async fn unconfigured_metering_refuses() {
        let metering = NoMeteringConfigured;
        let record = UsageRecord::new(
            "p",
            "s",
            MeterState::Running,
            0,
            1,
            ResourceUsage::default(),
        );
        assert!(matches!(
            metering.record(record).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            metering.summary("p").await,
            Err(SeamError::Unavailable(_))
        ));
    }
}
