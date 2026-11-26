//! The default reflection engine: pure folds plus an optional real re-run.
//!
//! [`DeterministicReflection`] is the production default because every step is
//! reproducible offline:
//!
//! * `analyze` → [`analyze`](agent_core::session::insight::analyze), a pure fold;
//! * `propose` → the rule table in [`propose`](super::propose);
//! * `validate` → the replay in [`validate`](super::validate).
//!
//! The only non-deterministic, cost-bearing step is a real re-run, so it is an
//! injected trait and a separate switch. Enabling `real_rerun` without wiring a
//! probe is a configuration error rather than a silent downgrade — a deployment
//! that believes it is double-checking must not silently single-check.

use crate::propose::propose_for_report;
use crate::validate::ReplayValidator;
use agent_core::seams::{
    ReflectionEngine, RerunProbe, RsiConfig, RsiScope, SeamError, SeamResult, ValidationOutcome,
};
use agent_core::session::insight::{analyze, EfficiencyReport, InsightLimits};
use agent_core::session::SessionRecord;
use async_trait::async_trait;
use std::sync::Arc;

/// Rule-table reflection engine.
#[derive(Clone)]
pub struct DeterministicReflection {
    config: RsiConfig,
    limits: InsightLimits,
    validator: ReplayValidator,
    rerun: Option<Arc<dyn RerunProbe>>,
}

impl DeterministicReflection {
    pub fn new(config: RsiConfig) -> Self {
        Self {
            limits: InsightLimits::from_config(&config),
            validator: ReplayValidator::new(config.min_score_gain),
            rerun: None,
            config,
        }
    }

    /// Attach the optional real re-run gate.
    pub fn with_rerun(mut self, probe: Arc<dyn RerunProbe>) -> Self {
        self.rerun = Some(probe);
        self
    }

    pub fn config(&self) -> &RsiConfig {
        &self.config
    }

    pub fn limits(&self) -> &InsightLimits {
        &self.limits
    }
}

#[async_trait]
impl ReflectionEngine for DeterministicReflection {
    async fn analyze(
        &self,
        session: &str,
        records: &[SessionRecord],
    ) -> SeamResult<EfficiencyReport> {
        Ok(analyze(session, records, &self.limits))
    }

    async fn propose(
        &self,
        report: &EfficiencyReport,
        scope: &RsiScope,
    ) -> SeamResult<Vec<agent_core::seams::Proposal>> {
        propose_for_report(report, scope, &self.config)
    }

    async fn validate(
        &self,
        proposal: &agent_core::seams::Proposal,
        report: &EfficiencyReport,
    ) -> SeamResult<ValidationOutcome> {
        let offline = self.validator.validate(proposal, report);
        if !self.config.real_rerun {
            return Ok(offline);
        }
        if !offline.passed() {
            // No point paying for a re-run the free gate already rejected.
            return Ok(offline);
        }
        let probe = self.rerun.as_ref().ok_or_else(|| {
            SeamError::Unavailable(
                "rsi real_rerun is enabled but no re-run probe is wired; \
                 refusing to report a pass that was never re-verified"
                    .into(),
            )
        })?;
        probe.rerun(proposal, report.score).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::session::insight::InsightLimits as Limits;
    use agent_core::session::{SessionEvent, SessionRecord};

    fn failing_ledger() -> Vec<SessionRecord> {
        let call = |seq: u64, id: &str| SessionRecord {
            seq,
            at_ms: 0,
            event: SessionEvent::ItemAdded {
                item_id: id.into(),
                kind: "tool_call".into(),
                payload: serde_json::json!({"name": "shell", "arguments": {}}),
            },
        };
        let done = |seq: u64, id: &str| SessionRecord {
            seq,
            at_ms: 0,
            event: SessionEvent::ItemDone {
                item_id: id.into(),
                result: serde_json::json!(null),
                is_error: true,
                latency_ms: Some(7),
            },
        };
        vec![
            SessionRecord {
                seq: 0,
                at_ms: 0,
                event: SessionEvent::TurnStarted {
                    turn_id: "t1".into(),
                    input: "go".into(),
                },
            },
            call(1, "a"),
            done(2, "a"),
            call(3, "b"),
            done(4, "b"),
            SessionRecord {
                seq: 5,
                at_ms: 0,
                event: SessionEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    output: "ok".into(),
                },
            },
        ]
    }

    struct AlwaysPass;

    #[async_trait]
    impl RerunProbe for AlwaysPass {
        async fn rerun(
            &self,
            _proposal: &agent_core::seams::Proposal,
            baseline: i32,
        ) -> SeamResult<ValidationOutcome> {
            Ok(ValidationOutcome::Passed {
                score: baseline + 30,
                baseline,
                replay: Default::default(),
            })
        }
    }

    #[tokio::test]
    async fn analyze_and_propose_run_entirely_offline() {
        let engine = DeterministicReflection::new(RsiConfig::default());
        assert_eq!(engine.limits(), &Limits::from_config(&RsiConfig::default()));
        let report = engine.analyze("s1", &failing_ledger()).await.unwrap();
        assert_eq!(report.turns.len(), 1);
        // Two measured calls at 7 ms each: the turn reports their sum.
        assert_eq!(report.turns[0].measured_tools, 2);
        assert_eq!(report.turns[0].latency_ms, 14);
        let scope = RsiScope::new("t", "d");
        let proposals = engine.propose(&report, &scope).await.unwrap();
        assert!(!proposals.is_empty());
        let outcome = engine.validate(&proposals[0], &report).await.unwrap();
        assert!(outcome.passed());
    }

    #[tokio::test]
    async fn a_rejected_proposal_never_reaches_the_paid_gate() {
        let config = RsiConfig {
            real_rerun: true,
            min_score_gain: 1_000,
            ..RsiConfig::default()
        };
        let engine = DeterministicReflection::new(config.clone()).with_rerun(Arc::new(AlwaysPass));
        let report = engine.analyze("s1", &failing_ledger()).await.unwrap();
        let proposals = engine
            .propose(&report, &RsiScope::new("t", "d"))
            .await
            .unwrap();
        let outcome = engine.validate(&proposals[0], &report).await.unwrap();
        assert!(matches!(outcome, ValidationOutcome::BelowThreshold { .. }));
    }

    #[tokio::test]
    async fn an_enabled_rerun_without_a_probe_fails_closed() {
        let config = RsiConfig {
            real_rerun: true,
            ..RsiConfig::default()
        };
        let engine = DeterministicReflection::new(config);
        let report = engine.analyze("s1", &failing_ledger()).await.unwrap();
        let proposals = engine
            .propose(&report, &RsiScope::new("t", "d"))
            .await
            .unwrap();
        assert!(matches!(
            engine.validate(&proposals[0], &report).await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_wired_probe_confirms_the_pass() {
        let config = RsiConfig {
            real_rerun: true,
            ..RsiConfig::default()
        };
        let engine = DeterministicReflection::new(config);
        assert!(engine.config().real_rerun);
        let engine = engine.with_rerun(Arc::new(AlwaysPass));
        let report = engine.analyze("s1", &failing_ledger()).await.unwrap();
        let proposals = engine
            .propose(&report, &RsiScope::new("t", "d"))
            .await
            .unwrap();
        let verdict = engine.validate(&proposals[0], &report).await.unwrap();
        assert!(verdict.passed(), "{verdict:?}");
        if let ValidationOutcome::Passed {
            score, baseline, ..
        } = &verdict
        {
            assert_eq!(*score, baseline + 30);
        }
    }
}
