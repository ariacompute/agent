//! The sweep: one bounded pass from "this session looks unhealthy" to
//! "a validated playbook is pending or published".
//!
//! Design constraints, each of which is a test below:
//!
//! * **Single flight.** Two sweeps must not analyse the same session twice and
//!   race each other into a revision conflict, so a concurrent call returns
//!   [`SweepOutcome::busy`] instead of blocking a request thread.
//! * **Fail soft on one session, hard on configuration.** A single bad session
//!   is logged and skipped; a disabled or invalid configuration is an error the
//!   caller can act on.
//! * **The gate is the only publication path.** `Human` never publishes, `Auto`
//!   publishes immediately (trusted deployments only), and `Broker` asks the
//!   existing approval seam — where anything other than `Allow`, including
//!   `Unavailable`, leaves the proposal pending.
//! * **Signal is consumed.** The session's unhealthy counter is reset only after
//!   a completed pass, so a failed sweep is retried instead of forgotten.

use crate::ledger::RsiLedger;
use crate::propose::ensure_evidenced;
use crate::skill::SkillSink;
use agent_core::seams::{
    ApprovalBroker, ApprovalRequest, Playbook, PlaybookCandidate, PlaybookStore, Proposal,
    ReflectionEngine, RsiConfig, RsiScope, SeamError, SeamResult, ValidationOutcome,
};
use agent_core::session::insight::EfficiencyReport;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Mutex;

/// Tool name used when the RSI gate consults an [`ApprovalBroker`].
pub const PROMOTE_TOOL: &str = "rsi_promote";

/// Who may publish a validated proposal.
#[derive(Clone)]
pub enum PromotionGate {
    /// Nobody automatically: only an explicit `promote` call publishes.
    Human,
    /// Trusted deployment: publish as soon as validation passes.
    Auto,
    /// Ask the approval seam; only `Allow` publishes.
    Broker(Arc<dyn ApprovalBroker>),
}

impl PromotionGate {
    /// Stable label for logs and API responses (never a secret).
    pub fn label(&self) -> &'static str {
        match self {
            PromotionGate::Human => "human",
            PromotionGate::Auto => "auto",
            PromotionGate::Broker(_) => "broker",
        }
    }

    /// Fail-closed: every outcome except `Allow` means "stay pending".
    pub async fn allows(&self, scope: &RsiScope, key: &str) -> bool {
        match self {
            PromotionGate::Human => false,
            PromotionGate::Auto => true,
            PromotionGate::Broker(broker) => {
                let request = ApprovalRequest::new(
                    PROMOTE_TOOL,
                    scope.principal.clone(),
                    serde_json::json!({"key": key, "domain": scope.domain}),
                );
                broker.decide(&request).await.is_allowed()
            }
        }
    }
}

impl Default for PromotionGate {
    /// The safe default for an unconfigured deployment.
    fn default() -> Self {
        PromotionGate::Human
    }
}

/// Durable home for a diagnosis.
///
/// The ledger is a bounded cache; a report is the artifact a reviewer has to be
/// able to open next week, so it is handed to a sink. Best effort: a report that
/// cannot be stored must not abort the sweep that produced it.
#[async_trait::async_trait]
pub trait ReportSink: Send + Sync {
    async fn record(
        &self,
        scope: &RsiScope,
        session: &str,
        report: &EfficiencyReport,
    ) -> SeamResult<()>;
}

/// Counters for one sweep. No content, no principal, no tool arguments.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepOutcome {
    /// Another sweep was already running; nothing was analysed.
    pub busy: bool,
    /// Sessions that had accumulated enough signal.
    pub sessions: usize,
    /// Reports produced.
    pub reports: usize,
    /// Candidates the engine proposed.
    pub proposals: usize,
    /// Candidates that passed validation and were queued for a decision.
    pub submitted: usize,
    /// Candidates published during this sweep.
    pub promoted: usize,
    /// Candidates dropped (no evidence, below threshold, or refused by the store).
    pub discarded: usize,
}

/// Orchestrates ledger → diagnosis → proposal → validation → gate → publish.
pub struct RsiSweep {
    ledger: Arc<RsiLedger>,
    engine: Arc<dyn ReflectionEngine>,
    playbooks: Arc<dyn PlaybookStore>,
    skills: Option<Arc<dyn SkillSink>>,
    reports: Option<Arc<dyn ReportSink>>,
    gate: PromotionGate,
    config: RsiConfig,
    flight: Mutex<()>,
}

impl RsiSweep {
    /// Build with the gate implied by `config.auto_promote`.
    pub fn new(
        ledger: Arc<RsiLedger>,
        engine: Arc<dyn ReflectionEngine>,
        playbooks: Arc<dyn PlaybookStore>,
        config: RsiConfig,
    ) -> Self {
        let gate = if config.auto_promote {
            PromotionGate::Auto
        } else {
            PromotionGate::Human
        };
        Self {
            ledger,
            engine,
            playbooks,
            skills: None,
            reports: None,
            gate,
            config,
            flight: Mutex::new(()),
        }
    }

    /// Replace the gate (an embedded deployment can wire an approval broker).
    pub fn with_gate(mut self, gate: PromotionGate) -> Self {
        self.gate = gate;
        self
    }

    /// Also distil tool-specific playbooks into `SKILL.md` files.
    pub fn with_skills(mut self, sink: Arc<dyn SkillSink>) -> Self {
        self.skills = Some(sink);
        self
    }

    /// Persist every diagnosis to `sink` (a failed write is logged, not fatal).
    pub fn with_reports(mut self, sink: Arc<dyn ReportSink>) -> Self {
        self.reports = Some(sink);
        self
    }

    pub fn config(&self) -> &RsiConfig {
        &self.config
    }

    pub fn gate(&self) -> &PromotionGate {
        &self.gate
    }

    pub fn ledger(&self) -> &Arc<RsiLedger> {
        &self.ledger
    }

    /// Sweep every session of one tenant that reached the signal threshold.
    pub async fn sweep_principal(&self, scope: &RsiScope) -> SeamResult<SweepOutcome> {
        if !self.config.enabled {
            return Err(SeamError::Unavailable(
                "recursive self-improvement is disabled (rsi.enabled = false)".into(),
            ));
        }
        scope.validate()?;
        let Some(_flight) = self.flight.try_lock().ok() else {
            return Ok(SweepOutcome {
                busy: true,
                ..SweepOutcome::default()
            });
        };
        let sessions = self.ledger.due(&scope.principal);
        self.run(scope, sessions).await
    }

    /// Sweep one named session, whatever its accumulated signal.
    pub async fn sweep_session(&self, scope: &RsiScope, session: &str) -> SeamResult<SweepOutcome> {
        if !self.config.enabled {
            return Err(SeamError::Unavailable(
                "recursive self-improvement is disabled (rsi.enabled = false)".into(),
            ));
        }
        scope.validate()?;
        if session.trim().is_empty() {
            return Err(SeamError::Invalid("session id is empty".into()));
        }
        let Some(_flight) = self.flight.try_lock().ok() else {
            return Ok(SweepOutcome {
                busy: true,
                ..SweepOutcome::default()
            });
        };
        self.run(scope, vec![session.to_string()]).await
    }

    async fn run(&self, scope: &RsiScope, sessions: Vec<String>) -> SeamResult<SweepOutcome> {
        let mut outcome = SweepOutcome::default();
        for session in sessions {
            outcome.sessions += 1;
            self.sweep_one(scope, &session, &mut outcome).await?;
        }
        Ok(outcome)
    }

    async fn sweep_one(
        &self,
        scope: &RsiScope,
        session: &str,
        outcome: &mut SweepOutcome,
    ) -> SeamResult<()> {
        let records = self.ledger.records(&scope.principal, session);
        if records.is_empty() {
            return Ok(());
        }
        let report = self.engine.analyze(session, &records).await?;
        outcome.reports += 1;
        if let Some(sink) = self.reports.as_ref() {
            if let Err(e) = sink.record(scope, session, &report).await {
                // The diagnosis still drives the sweep; only its audit copy is
                // missing, and the ledger can reproduce it.
                tracing::warn!(session, error = %e, "RSI report could not be stored");
            }
        }
        let proposals = self.engine.propose(&report, scope).await?;
        for proposal in proposals {
            outcome.proposals += 1;
            if let Err(e) = ensure_evidenced(&proposal) {
                tracing::warn!(session, error = %e, "dropping an RSI proposal with no evidence");
                outcome.discarded += 1;
                continue;
            }
            let score = match self.engine.validate(&proposal, &report).await? {
                ValidationOutcome::Passed { score, .. } => score,
                other => {
                    // The counter is the observable signal; `verdict.as_str()`
                    // stays available to a caller that wants the reason.
                    debug_assert!(!other.passed());
                    outcome.discarded += 1;
                    continue;
                }
            };
            let candidate = PlaybookCandidate {
                proposal_id: proposal.id.clone(),
                origin: proposal.scope.clone(),
                scope: proposal.scope.clone(),
                key: proposal.key(),
                title: proposal.title.clone(),
                body: proposal.body.clone(),
                score,
                baseline: proposal.baseline,
                evidence: proposal.evidence.clone(),
                adaptation: proposal.adaptation.clone(),
            };
            if let Err(e) = self.playbooks.submit(candidate).await {
                tracing::warn!(session, error = %e, "RSI proposal refused by the playbook store");
                outcome.discarded += 1;
                continue;
            }
            outcome.submitted += 1;
            if !self.gate.allows(scope, &proposal.key()).await {
                continue;
            }
            let current = self
                .playbooks
                .current(scope, &proposal.key())
                .await?
                .map(|playbook| playbook.revision)
                .unwrap_or(0);
            match self.playbooks.promote(&proposal.id, current).await {
                Ok(playbook) => {
                    outcome.promoted += 1;
                    self.distill_skill(&proposal, &playbook).await;
                }
                Err(e) => {
                    // The proposal stays pending; a human or the next sweep can
                    // retry against the fresh revision.
                    tracing::warn!(key = %proposal.key(), error = %e, "RSI promotion did not apply");
                }
            }
        }
        // Only a completed pass consumes the signal.
        self.ledger.reset(&scope.principal, session);
        Ok(())
    }

    async fn distill_skill(&self, proposal: &Proposal, playbook: &Playbook) {
        let (Some(sink), Some(tool)) = (self.skills.as_ref(), proposal.tool.as_ref()) else {
            return;
        };
        let name = skill_name(&proposal.pathology, tool);
        if let Err(e) = sink
            .write_skill(&name, &playbook.title, &playbook.body)
            .await
        {
            // The playbook is already durable; a missing mirror is a warning.
            tracing::warn!(skill = %name, error = %e, "distilled skill could not be written");
        }
    }
}

/// `rsi-<pathology>-<tool>`; the sink sanitises it further.
pub fn skill_name(pathology: &str, tool: &str) -> String {
    format!("rsi-{pathology}-{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::DeterministicReflection;
    use crate::playbook::MemoryPlaybooks;
    use crate::skill::FilesystemSkillSink;
    use agent_core::seams::{AutoAllow, DenyAll, Proposal};
    use agent_core::session::insight::EfficiencyReport;
    use agent_core::session::{SessionEvent, SessionRecord};
    use async_trait::async_trait;

    fn config() -> RsiConfig {
        RsiConfig {
            enabled: true,
            sweep_threshold: 1,
            ..RsiConfig::default()
        }
    }

    fn failing_done(seq: u64, item_id: &str) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: seq as i64,
            event: SessionEvent::ItemDone {
                item_id: item_id.into(),
                result: serde_json::json!({"ok": false}),
                is_error: true,
                latency_ms: Some(20),
            },
        }
    }

    fn failing_session() -> Vec<SessionRecord> {
        vec![
            SessionRecord {
                seq: 0,
                at_ms: 0,
                event: SessionEvent::TurnStarted {
                    turn_id: "t1".into(),
                    input: "go".into(),
                },
            },
            SessionRecord {
                seq: 1,
                at_ms: 1,
                event: SessionEvent::ItemAdded {
                    item_id: "a".into(),
                    kind: "tool_call".into(),
                    payload: serde_json::json!({"name": "shell", "arguments": {}}),
                },
            },
            failing_done(2, "a"),
            SessionRecord {
                seq: 3,
                at_ms: 3,
                event: SessionEvent::ItemAdded {
                    item_id: "b".into(),
                    kind: "tool_call".into(),
                    payload: serde_json::json!({"name": "shell", "arguments": {}}),
                },
            },
            failing_done(4, "b"),
            SessionRecord {
                seq: 5,
                at_ms: 5,
                event: SessionEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    output: "ok".into(),
                },
            },
        ]
    }

    async fn prepared(
        gate: PromotionGate,
    ) -> (
        RsiSweep,
        Arc<MemoryPlaybooks>,
        Arc<RsiLedger>,
        tempfile::TempDir,
    ) {
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let playbooks = Arc::new(MemoryPlaybooks::new());
        let dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(FilesystemSkillSink::new(dir.path().join("skills")));
        let sweep = RsiSweep::new(
            ledger.clone(),
            Arc::new(DeterministicReflection::new(cfg)),
            playbooks.clone(),
            config(),
        )
        .with_gate(gate)
        .with_skills(sink);
        (sweep, playbooks, ledger, dir)
    }

    fn scope() -> RsiScope {
        RsiScope::new("tenant", "orders")
    }

    #[tokio::test]
    async fn the_human_gate_queues_but_never_publishes() {
        let (sweep, playbooks, ledger, dir) = prepared(PromotionGate::Human).await;
        assert_eq!(sweep.gate().label(), "human");
        assert!(sweep.config().enabled);
        assert_eq!(sweep.ledger().stats().sessions, 1);

        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(!outcome.busy);
        assert_eq!(outcome.sessions, 1);
        assert_eq!(outcome.reports, 1);
        assert!(outcome.proposals >= 1);
        assert_eq!(outcome.submitted, outcome.proposals);
        assert_eq!(outcome.promoted, 0);
        assert_eq!(playbooks.pending_count(), outcome.submitted);
        assert!(
            !dir.path().join("skills").exists(),
            "no skill before promotion"
        );

        // The signal was consumed, so an immediate second sweep is a no-op.
        let again = sweep.sweep_principal(&scope()).await.unwrap();
        assert_eq!(again.sessions, 0);
        assert_eq!(ledger.stats().unhealthy_sessions, 0);
    }

    #[tokio::test]
    async fn the_auto_gate_publishes_and_distils_a_skill() {
        let (sweep, playbooks, _, dir) = prepared(PromotionGate::Auto).await;
        assert_eq!(sweep.gate().label(), "auto");
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.promoted >= 1);
        assert_eq!(playbooks.pending_count(), 0);
        let current = playbooks
            .current(&scope(), "repeated_tool_failure:shell")
            .await
            .unwrap()
            .expect("tool playbook published");
        assert_eq!(current.revision, 1);
        let skill = dir
            .path()
            .join("skills/rsi-repeated_tool_failure-shell/SKILL.md");
        assert!(skill.exists(), "tool playbooks are distilled into a skill");
        assert!(std::fs::read_to_string(skill)
            .unwrap()
            .contains(&current.title));
    }

    #[tokio::test]
    async fn the_broker_gate_is_fail_closed() {
        let (sweep, playbooks, _, _) = prepared(PromotionGate::Broker(Arc::new(DenyAll))).await;
        assert_eq!(sweep.gate().label(), "broker");
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.submitted >= 1);
        assert_eq!(outcome.promoted, 0);
        assert_eq!(
            playbooks.pending_count(),
            outcome.submitted,
            "an unavailable decision leaves the proposal pending"
        );

        let (allowing, allowed_store, _, _) =
            prepared(PromotionGate::Broker(Arc::new(AutoAllow))).await;
        let outcome = allowing.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.promoted >= 1);
        assert_eq!(allowed_store.pending_count(), 0);
    }

    #[tokio::test]
    async fn auto_promote_configuration_selects_the_auto_gate() {
        let cfg = RsiConfig {
            enabled: true,
            auto_promote: true,
            sweep_threshold: 1,
            ..RsiConfig::default()
        };
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            Arc::new(MemoryPlaybooks::new()),
            cfg,
        );
        assert_eq!(sweep.gate().label(), "auto");
        assert_eq!(PromotionGate::default().label(), "human");
    }

    #[tokio::test]
    async fn a_disabled_or_invalid_configuration_is_refused() {
        let cfg = RsiConfig::default();
        let sweep = RsiSweep::new(
            Arc::new(RsiLedger::new(&cfg)),
            Arc::new(DeterministicReflection::new(cfg.clone())),
            Arc::new(MemoryPlaybooks::new()),
            cfg,
        );
        assert!(matches!(
            sweep.sweep_principal(&scope()).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            sweep.sweep_session(&scope(), "s1").await,
            Err(SeamError::Unavailable(_))
        ));

        let (enabled, _, _, _) = prepared(PromotionGate::Human).await;
        assert!(matches!(
            enabled.sweep_principal(&RsiScope::new("", "d")).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            enabled.sweep_session(&scope(), "  ").await,
            Err(SeamError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn a_concurrent_sweep_reports_busy_instead_of_double_analysing() {
        let (sweep, _, _, _) = prepared(PromotionGate::Human).await;
        let _flight = sweep.flight.lock().await;
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.busy);
        assert_eq!(outcome.sessions, 0);
        let busy = sweep.sweep_session(&scope(), "s1").await.unwrap();
        assert!(busy.busy);
    }

    #[tokio::test]
    async fn an_explicit_session_sweep_ignores_the_signal_threshold() {
        let cfg = RsiConfig {
            enabled: true,
            sweep_threshold: 99,
            ..RsiConfig::default()
        };
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let sweep = RsiSweep::new(
            ledger.clone(),
            Arc::new(DeterministicReflection::new(cfg.clone())),
            Arc::new(MemoryPlaybooks::new()),
            cfg,
        );
        assert!(sweep.sweep_principal(&scope()).await.unwrap().sessions == 0);
        let outcome = sweep.sweep_session(&scope(), "s1").await.unwrap();
        assert_eq!(outcome.sessions, 1);
        assert_eq!(outcome.reports, 1);
        // An unknown session is simply empty, not an error.
        assert_eq!(
            sweep
                .sweep_session(&scope(), "missing")
                .await
                .unwrap()
                .reports,
            0
        );
        assert_eq!(ledger.stats().sessions, 1);
    }

    /// Engine that returns exactly what the test asks for, so the rejection
    /// paths are reachable without contriving a ledger.
    struct FixedEngine {
        proposals: Vec<Proposal>,
        verdict: ValidationOutcome,
        fail_analyze: bool,
    }

    #[async_trait]
    impl ReflectionEngine for FixedEngine {
        async fn analyze(
            &self,
            session: &str,
            _records: &[SessionRecord],
        ) -> SeamResult<EfficiencyReport> {
            if self.fail_analyze {
                return Err(SeamError::Io("ledger unreadable".into()));
            }
            Ok(EfficiencyReport {
                session: session.into(),
                folded_records: 1,
                truncated: false,
                turns: Vec::new(),
                findings: Vec::new(),
                score: 50,
            })
        }

        async fn propose(
            &self,
            _report: &EfficiencyReport,
            _scope: &RsiScope,
        ) -> SeamResult<Vec<Proposal>> {
            Ok(self.proposals.clone())
        }

        async fn validate(
            &self,
            _proposal: &Proposal,
            _report: &EfficiencyReport,
        ) -> SeamResult<ValidationOutcome> {
            Ok(self.verdict.clone())
        }
    }

    fn fixed_proposal(evidence: bool) -> Proposal {
        Proposal {
            id: "p1".into(),
            scope: scope(),
            origin: scope(),
            pathology: "retry_storm".into(),
            tool: Some("shell".into()),
            title: "t".into(),
            body: "b".into(),
            score: 90,
            baseline: 50,
            evidence: if evidence {
                vec![agent_core::session::insight::Evidence {
                    seq: 1,
                    turn_id: None,
                    tool: None,
                    detail: "x".into(),
                }]
            } else {
                Vec::new()
            },
            created_at_ms: 0,
            adaptation: None,
        }
    }

    async fn with_engine(engine: Arc<dyn ReflectionEngine>) -> (RsiSweep, Arc<MemoryPlaybooks>) {
        let ledger = Arc::new(RsiLedger::new(&config()));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let playbooks = Arc::new(MemoryPlaybooks::new());
        let sweep = RsiSweep::new(ledger, engine, playbooks.clone(), config())
            .with_gate(PromotionGate::Auto);
        (sweep, playbooks)
    }

    #[tokio::test]
    async fn unevidenced_proposals_are_discarded() {
        let engine = Arc::new(FixedEngine {
            proposals: vec![fixed_proposal(false)],
            verdict: ValidationOutcome::Passed {
                score: 90,
                baseline: 50,
                replay: Default::default(),
            },
            fail_analyze: false,
        });
        let (sweep, playbooks) = with_engine(engine).await;
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert_eq!(outcome.proposals, 1);
        assert_eq!(outcome.discarded, 1);
        assert_eq!(outcome.submitted, 0);
        assert_eq!(playbooks.pending_count(), 0);
    }

    #[tokio::test]
    async fn rejected_proposals_are_discarded() {
        let engine = Arc::new(FixedEngine {
            proposals: vec![fixed_proposal(true)],
            verdict: ValidationOutcome::BelowThreshold {
                score: 51,
                baseline: 50,
                required: 5,
            },
            fail_analyze: false,
        });
        let (sweep, playbooks) = with_engine(engine).await;
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert_eq!(outcome.discarded, 1);
        assert_eq!(outcome.submitted, 0);
        assert_eq!(playbooks.pending_count(), 0);
    }

    #[tokio::test]
    async fn a_store_that_refuses_the_candidate_counts_as_discarded() {
        // A buggy engine reports a "pass" that does not actually improve the
        // baseline; the store's own validation must still refuse to queue it.
        let engine = Arc::new(FixedEngine {
            proposals: vec![fixed_proposal(true)],
            verdict: ValidationOutcome::Passed {
                score: 50,
                baseline: 50,
                replay: Default::default(),
            },
            fail_analyze: false,
        });
        let (sweep, playbooks) = with_engine(engine).await;
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert_eq!(outcome.submitted, 0);
        assert_eq!(outcome.discarded, 1);
        assert_eq!(playbooks.pending_count(), 0);
    }

    #[tokio::test]
    async fn a_failing_engine_surfaces_and_keeps_the_signal_for_a_retry() {
        let engine = Arc::new(FixedEngine {
            proposals: Vec::new(),
            verdict: ValidationOutcome::Inconclusive {
                reason: "n/a".into(),
            },
            fail_analyze: true,
        });
        let (sweep, _) = with_engine(engine).await;
        assert!(matches!(
            sweep.sweep_principal(&scope()).await,
            Err(SeamError::Io(_))
        ));
        assert_eq!(
            sweep.ledger().stats().unhealthy_sessions,
            1,
            "a failed sweep must not consume the signal"
        );
    }

    struct RecordingSink {
        seen: std::sync::Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    #[async_trait]
    impl ReportSink for RecordingSink {
        async fn record(
            &self,
            scope: &RsiScope,
            session: &str,
            _report: &EfficiencyReport,
        ) -> SeamResult<()> {
            if self.fail {
                return Err(SeamError::Io("disk full".into()));
            }
            self.seen
                .lock()
                .unwrap()
                .push((scope.principal.clone(), session.to_string()));
            Ok(())
        }
    }

    #[tokio::test]
    async fn diagnoses_are_handed_to_the_report_sink() {
        let sink = Arc::new(RecordingSink {
            seen: std::sync::Mutex::new(Vec::new()),
            fail: false,
        });
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            Arc::new(MemoryPlaybooks::new()),
            cfg,
        )
        .with_reports(sink.clone());
        sweep.sweep_principal(&scope()).await.unwrap();
        let seen = sink.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].0, "tenant");
        assert_eq!(seen[0].1, "s1");
    }

    #[tokio::test]
    async fn a_failing_report_sink_does_not_abort_the_sweep() {
        let sink = Arc::new(RecordingSink {
            seen: std::sync::Mutex::new(Vec::new()),
            fail: true,
        });
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let playbooks = Arc::new(MemoryPlaybooks::new());
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            playbooks.clone(),
            cfg,
        )
        .with_gate(PromotionGate::Auto)
        .with_reports(sink);
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert_eq!(outcome.reports, 1);
        assert!(
            outcome.promoted >= 1,
            "publication does not depend on the audit copy"
        );
        assert_eq!(playbooks.pending_count(), 0);
    }

    /// A skill sink that always fails, to prove a missing mirror is not fatal.
    struct FailingSkills;

    #[async_trait]
    impl SkillSink for FailingSkills {
        async fn write_skill(
            &self,
            _name: &str,
            _title: &str,
            _body: &str,
        ) -> SeamResult<std::path::PathBuf> {
            Err(SeamError::Io("read-only workspace".into()))
        }
    }

    #[tokio::test]
    async fn a_failing_skill_sink_does_not_undo_a_publication() {
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let playbooks = Arc::new(MemoryPlaybooks::new());
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            playbooks.clone(),
            cfg,
        )
        .with_gate(PromotionGate::Auto)
        .with_skills(Arc::new(FailingSkills));
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.promoted >= 1);
        assert!(playbooks
            .current(&scope(), "repeated_tool_failure:shell")
            .await
            .unwrap()
            .is_some());
    }

    /// Reports a revision that is always ahead, so `promote` conflicts.
    struct LyingPlaybooks {
        inner: Arc<MemoryPlaybooks>,
    }

    #[async_trait]
    impl PlaybookStore for LyingPlaybooks {
        async fn submit(&self, candidate: PlaybookCandidate) -> SeamResult<Proposal> {
            self.inner.submit(candidate).await
        }

        async fn pending(&self, scope: &RsiScope) -> SeamResult<Vec<Proposal>> {
            self.inner.pending(scope).await
        }

        async fn promote(&self, id: &str, expected: u64) -> SeamResult<Playbook> {
            self.inner.promote(id, expected).await
        }

        async fn discard(&self, id: &str) -> SeamResult<Proposal> {
            self.inner.discard(id).await
        }

        async fn current(&self, scope: &RsiScope, key: &str) -> SeamResult<Option<Playbook>> {
            Ok(Some(Playbook {
                key: key.to_string(),
                scope: scope.clone(),
                origin: scope.clone(),
                revision: 99,
                title: "elsewhere".into(),
                body: "b".into(),
                score: 100,
                source_proposal: "other".into(),
                adaptation: None,
                updated_at_ms: 0,
            }))
        }

        async fn list_versions(&self, scope: &RsiScope, key: &str) -> SeamResult<Vec<Playbook>> {
            self.inner.list_versions(scope, key).await
        }
    }

    #[tokio::test]
    async fn a_promotion_that_conflicts_keeps_the_proposal_pending() {
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let inner = Arc::new(MemoryPlaybooks::new());
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            Arc::new(LyingPlaybooks {
                inner: inner.clone(),
            }),
            cfg,
        )
        .with_gate(PromotionGate::Auto);
        // The wrapper is faithful for everything the sweep does not override.
        assert!(Arc::new(LyingPlaybooks {
            inner: inner.clone()
        })
        .pending(&scope())
        .await
        .unwrap()
        .is_empty());
        assert!(Arc::new(LyingPlaybooks {
            inner: inner.clone()
        })
        .list_versions(&scope(), "k")
        .await
        .unwrap()
        .is_empty());
        assert!(Arc::new(LyingPlaybooks {
            inner: inner.clone()
        })
        .discard("nope")
        .await
        .is_err());

        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.submitted >= 1);
        assert_eq!(outcome.promoted, 0, "a conflict is not a publication");
        assert_eq!(inner.pending_count(), outcome.submitted);
    }

    #[tokio::test]
    async fn a_second_publication_advances_the_revision() {
        let cfg = config();
        let ledger = Arc::new(RsiLedger::new(&cfg));
        ledger.append("tenant", "s1", &failing_session()).unwrap();
        let playbooks = Arc::new(MemoryPlaybooks::new());
        playbooks.submit(fixed_candidate("seed")).await.unwrap();
        playbooks.promote("seed", 0).await.unwrap();
        let sweep = RsiSweep::new(
            ledger,
            Arc::new(DeterministicReflection::new(cfg.clone())),
            playbooks.clone(),
            cfg,
        )
        .with_gate(PromotionGate::Auto);
        let outcome = sweep.sweep_principal(&scope()).await.unwrap();
        assert!(outcome.promoted >= 1);
        let current = playbooks
            .current(&scope(), "repeated_tool_failure:shell")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(current.revision, 2);
    }

    fn fixed_candidate(id: &str) -> PlaybookCandidate {
        PlaybookCandidate {
            proposal_id: id.into(),
            origin: scope(),
            scope: scope(),
            key: "repeated_tool_failure:shell".into(),
            title: "seed".into(),
            body: "seed".into(),
            score: 90,
            baseline: 70,
            evidence: Vec::new(),
            adaptation: None,
        }
    }

    #[test]
    fn skill_names_are_derived_from_the_pathology_and_tool() {
        assert_eq!(
            skill_name("retry_storm", "web_search"),
            "rsi-retry_storm-web_search"
        );
    }
}
