//! Metering ledger, price book and the **shared** quota gate.
//!
//! Two invariants (ADR-0022):
//! 1. `spawn` and `restore` call [`MeteringLedger::reserve`] — the same function.
//!    There is no second code path, so restoring from a checkpoint cannot exceed
//!    the quota that creating a sandbox enforces.
//! 2. Estimates never become money. [`agent_core::seams::EstimatedTokens`] is
//!    not accepted by any method here; only [`BilledTokens`] and measured
//!    resource-milliseconds are.

use agent_core::seams::{
    BilledTokens, MeteringSeam, ResourceRequest, ResourceUsage, SeamError, SeamResult, UsageRecord,
    UsageSummary,
};
use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::Mutex;

/// Rates in **micro-USD** per unit-hour. Integer arithmetic only.
///
/// The default is all zeros on purpose: no vendor price is hardcoded, so a
/// deployment must opt into a rate card before any spend is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PriceBook {
    pub cpu_micros_per_vcpu_hour: u64,
    pub memory_micros_per_gib_hour: u64,
    pub disk_micros_per_gib_hour: u64,
}

impl PriceBook {
    pub fn is_zero(&self) -> bool {
        self.cpu_micros_per_vcpu_hour == 0
            && self.memory_micros_per_gib_hour == 0
            && self.disk_micros_per_gib_hour == 0
    }

    /// Rated spend for one usage bundle, in micro-USD.
    pub fn rate(&self, usage: &ResourceUsage) -> u64 {
        let cpu = micros(usage.vcpu_millis, 1, self.cpu_micros_per_vcpu_hour);
        let memory = micros(
            usage.memory_mib_millis,
            1024,
            self.memory_micros_per_gib_hour,
        );
        let disk = micros(usage.disk_mib_millis, 1024, self.disk_micros_per_gib_hour);
        cpu.saturating_add(memory).saturating_add(disk)
    }
}

/// `unit_millis` is "unit × ms"; dividing by `unit_divisor` gives the billed
/// unit, then by 3_600_000 ms/h gives unit-hours.
fn micros(unit_millis: u64, unit_divisor: u64, micros_per_unit_hour: u64) -> u64 {
    let numerator = (unit_millis as u128).saturating_mul(micros_per_unit_hour as u128);
    let denominator = (unit_divisor as u128).saturating_mul(3_600_000);
    (numerator / denominator) as u64
}

/// A weight-based quota. Weights are shape-derived (see
/// [`ResourceRequest::weight`]) so vCPU, memory and disk cannot be traded off
/// against each other by a caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaPolicy {
    pub max_weight: u64,
}

impl QuotaPolicy {
    pub fn new(max_weight: u64) -> Self {
        Self { max_weight }
    }
}

/// In-memory ledger. A Postgres-backed implementation lives in
/// `aria-agent-cloud`; both share this trait and this arithmetic.
#[derive(Debug, Default)]
pub struct MeteringLedger {
    records: Mutex<Vec<UsageRecord>>,
    reservations: Mutex<HashMap<String, u64>>,
    price_book: Option<PriceBook>,
    quota: Option<QuotaPolicy>,
}

impl MeteringLedger {
    pub fn new(price_book: Option<PriceBook>, quota: Option<QuotaPolicy>) -> Self {
        Self {
            records: Mutex::new(Vec::new()),
            reservations: Mutex::new(HashMap::new()),
            price_book,
            quota,
        }
    }

    /// Current reserved weight for one principal.
    pub fn reserved_weight(&self, principal_id: &str) -> u64 {
        self.reservations
            .lock()
            .map(|r| r.get(principal_id).copied().unwrap_or(0))
            .unwrap_or(0)
    }

    /// The single quota gate used by both create and restore.
    pub fn check(
        &self,
        principal_id: &str,
        sandbox_id: &str,
        request: ResourceRequest,
    ) -> SeamResult<()> {
        let Some(quota) = self.quota else {
            return Ok(());
        };
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| SeamError::Io("metering lock poisoned".into()))?;
        let current = reservations.get(principal_id).copied().unwrap_or(0);
        // Re-reserving the same sandbox (retry) must be idempotent, not additive.
        let already = reservations
            .get(&format!("{principal_id}/{sandbox_id}"))
            .copied()
            .unwrap_or(0);
        let next = current
            .saturating_sub(already)
            .saturating_add(request.weight());
        if next > quota.max_weight {
            return Err(SeamError::Denied(format!(
                "quota exceeded for `{principal_id}`: {} of {} weight in use, \
                 this request needs {} (free capacity by destroying or shrinking sandboxes)",
                current,
                quota.max_weight,
                request.weight()
            )));
        }
        reservations.insert(principal_id.to_string(), next);
        reservations.insert(format!("{principal_id}/{sandbox_id}"), request.weight());
        Ok(())
    }
}

#[async_trait]
impl MeteringSeam for MeteringLedger {
    async fn record(&self, record: UsageRecord) -> SeamResult<()> {
        self.records
            .lock()
            .map_err(|_| SeamError::Io("metering lock poisoned".into()))?
            .push(record);
        Ok(())
    }

    async fn summary(&self, principal_id: &str) -> SeamResult<UsageSummary> {
        let records = self
            .records
            .lock()
            .map_err(|_| SeamError::Io("metering lock poisoned".into()))?;
        let mut summary = UsageSummary {
            principal_id: principal_id.to_string(),
            ..UsageSummary::default()
        };
        for record in records.iter().filter(|r| r.principal_id == principal_id) {
            summary.records += 1;
            summary.resources.merge(&record.resources);
            if let Some(tokens) = record.tokens {
                summary.tokens.input = summary.tokens.input.saturating_add(tokens.input);
                summary.tokens.output = summary.tokens.output.saturating_add(tokens.output);
            }
        }
        if let Some(book) = self.price_book {
            if !book.is_zero() {
                summary.priced_micros_usd = Some(book.rate(&summary.resources));
            }
        }
        Ok(summary)
    }

    async fn reserve(
        &self,
        principal_id: &str,
        sandbox_id: &str,
        request: ResourceRequest,
    ) -> SeamResult<()> {
        self.check(principal_id, sandbox_id, request)
    }

    async fn release(&self, principal_id: &str, sandbox_id: &str) -> SeamResult<()> {
        let mut reservations = self
            .reservations
            .lock()
            .map_err(|_| SeamError::Io("metering lock poisoned".into()))?;
        if let Some(weight) = reservations.remove(&format!("{principal_id}/{sandbox_id}")) {
            let current = reservations.get(principal_id).copied().unwrap_or(0);
            reservations.insert(principal_id.to_string(), current.saturating_sub(weight));
        }
        Ok(())
    }
}

/// A token-usage helper that carries the provider's numbers, never an estimate.
pub fn billed_tokens(input: u64, output: u64) -> BilledTokens {
    BilledTokens::new(input, output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::MeterState;

    fn request(vcpus: u64, memory_mib: u64, disk_mib: u64) -> ResourceRequest {
        ResourceRequest {
            vcpus_x1000: vcpus * 1000,
            memory_mib,
            disk_mib,
        }
    }

    #[test]
    fn pricing_is_integer_and_dimension_aware() {
        let book = PriceBook {
            cpu_micros_per_vcpu_hour: 50_400,
            memory_micros_per_gib_hour: 16_200,
            disk_micros_per_gib_hour: 108,
        };
        assert!(!book.is_zero());
        // 1 vCPU-hour.
        let usage = ResourceUsage {
            vcpu_millis: 3_600_000,
            ..ResourceUsage::default()
        };
        assert_eq!(book.rate(&usage), 50_400);
        // 1 GiB-hour.
        let usage = ResourceUsage {
            memory_mib_millis: 1024 * 3_600_000,
            ..ResourceUsage::default()
        };
        assert_eq!(book.rate(&usage), 16_200);
        // Half an hour of 2 vCPU = 1 vCPU-hour.
        let usage = ResourceUsage {
            vcpu_millis: 2 * 1_800_000,
            ..ResourceUsage::default()
        };
        assert_eq!(book.rate(&usage), 50_400);
        assert!(PriceBook::default().is_zero());
    }

    #[tokio::test]
    async fn quota_denies_with_an_actionable_message_and_is_idempotent() {
        let ledger = MeteringLedger::new(None, Some(QuotaPolicy::new(5_000)));
        let req = request(2, 1024, 10_240);
        let weight = req.weight();
        assert_eq!(weight, 2000 + 1024 + 1024);

        ledger.reserve("p", "s1", req).await.unwrap();
        assert_eq!(ledger.reserved_weight("p"), weight);
        // Re-reserving the same sandbox (a retry) does not double-count.
        ledger.reserve("p", "s1", req).await.unwrap();
        assert_eq!(ledger.reserved_weight("p"), weight);

        let err = ledger.reserve("p", "s2", req).await.unwrap_err();
        assert!(matches!(err, SeamError::Denied(_)), "{err:?}");
        assert!(err.to_string().contains("quota exceeded"), "{err}");

        ledger.release("p", "s1").await.unwrap();
        assert_eq!(ledger.reserved_weight("p"), 0);
        ledger.reserve("p", "s2", req).await.unwrap();
        ledger.release("p", "unknown").await.unwrap();
    }

    #[tokio::test]
    async fn a_restore_cannot_bypass_the_quota() {
        // One sandbox fits (4048 weight), two do not (8096 > 5000).
        let ledger = MeteringLedger::new(None, Some(QuotaPolicy::new(5_000)));
        let req = request(2, 1024, 10_240);
        ledger.reserve("p", "origin", req).await.unwrap();
        // Restoring derives a *new* sandbox and must pass the same gate.
        let err = ledger.reserve("p", "restored", req).await.unwrap_err();
        assert!(matches!(err, SeamError::Denied(_)));
        // Keeping the origin and shrinking the restore fits.
        ledger
            .reserve("p", "restored", request(0, 512, 0))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn no_quota_means_unlimited() {
        let ledger = MeteringLedger::default();
        for i in 0..10 {
            ledger
                .reserve("p", &format!("s{i}"), request(64, 65_536, 1_000_000))
                .await
                .unwrap();
        }
    }

    #[tokio::test]
    async fn summary_aggregates_resources_tokens_and_price() {
        let book = PriceBook {
            cpu_micros_per_vcpu_hour: 50_400,
            memory_micros_per_gib_hour: 16_200,
            disk_micros_per_gib_hour: 108,
        };
        let ledger = MeteringLedger::new(Some(book), None);
        let mut running = UsageRecord::new(
            "p",
            "s1",
            MeterState::Running,
            0,
            3_600_000,
            ResourceUsage::for_state(MeterState::Running, 1000, 1024, 0, 3_600_000),
        );
        running.tokens = Some(billed_tokens(100, 50));
        ledger.record(running).await.unwrap();
        ledger
            .record(UsageRecord::new(
                "p",
                "s1",
                MeterState::Suspended,
                3_600_000,
                7_200_000,
                ResourceUsage::for_state(MeterState::Suspended, 1000, 1024, 0, 3_600_000),
            ))
            .await
            .unwrap();
        ledger
            .record(UsageRecord::new(
                "other",
                "s2",
                MeterState::Running,
                0,
                10,
                ResourceUsage::default(),
            ))
            .await
            .unwrap();

        let summary = ledger.summary("p").await.unwrap();
        assert_eq!(summary.records, 2, "other principals do not leak in");
        assert_eq!(summary.tokens.total(), 150);
        assert_eq!(
            summary.resources.vcpu_millis, 3_600_000,
            "suspended has no vCPU"
        );
        assert_eq!(summary.resources.memory_mib_millis, 1024 * 3_600_000);
        assert_eq!(summary.priced_micros_usd, Some(50_400 + 16_200));
    }

    #[tokio::test]
    async fn zero_prices_produce_no_rated_spend() {
        let ledger = MeteringLedger::new(Some(PriceBook::default()), None);
        ledger
            .record(UsageRecord::new(
                "p",
                "s",
                MeterState::Running,
                0,
                1,
                ResourceUsage::default(),
            ))
            .await
            .unwrap();
        assert!(ledger
            .summary("p")
            .await
            .unwrap()
            .priced_micros_usd
            .is_none());
        assert_eq!(ledger.summary("nobody").await.unwrap().records, 0);
    }

    #[tokio::test]
    async fn a_poisoned_metering_lock_fails_closed() {
        let ledger = MeteringLedger::new(Some(PriceBook::default()), Some(QuotaPolicy::new(100)));
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ledger.reservations.lock().unwrap();
            panic!("poison the reservation lock");
        }));
        assert!(poisoned.is_err());
        let poisoned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = ledger.records.lock().unwrap();
            panic!("poison the record lock");
        }));
        assert!(poisoned.is_err());
        assert!(matches!(
            ledger.check("p", "s", ResourceRequest::default()),
            Err(SeamError::Io(_))
        ));
        assert!(matches!(
            ledger
                .record(UsageRecord::new(
                    "p",
                    "s",
                    MeterState::Running,
                    0,
                    1,
                    ResourceUsage::default()
                ))
                .await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(
            ledger.release("p", "s").await,
            Err(SeamError::Io(_))
        ));
        assert!(matches!(ledger.summary("p").await, Err(SeamError::Io(_))));
    }
}
