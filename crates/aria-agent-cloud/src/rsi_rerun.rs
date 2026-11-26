//! The real re-run gate: one actual model call, with a receipt.
//!
//! Offline replay is enough to *propose* something; this is the optional second
//! gate that spends money, so it is built around three rules:
//!
//! 1. **It really calls a model.** The probe replays the input of the turn the
//!    evidence came from, with the candidate playbook applied. It is not a
//!    second simulation of history.
//! 2. **It never invents a score.** A tool-free re-run cannot re-derive the
//!    replay gain, so a pass keeps the replay score and only *confirms* that a
//!    real turn under the playbook completes. An empty or failed turn is never a
//!    pass, and neither is a missing deadline.
//! 3. **Every attempt leaves a receipt.** Tokens, model round-trips, tool calls
//!    and duration go to `rsi_reruns` before the verdict is returned; a probe
//!    whose receipt cannot be written fails closed instead of promoting an
//!    unaccounted action.
//!
//! The probe is deliberately tool-free: a re-run that executed shell or file
//! tools would turn a validation into a second production incident. `tool_calls`
//! is therefore recorded as `0` — the absence of side effects is a fact the
//! receipt states, not an omission.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_core::seams::{
    Proposal, ReplayStats, RerunProbe, SeamError, SeamResult, ValidationOutcome,
};
use agent_core::session::SessionEvent;
use agent_core::session::SessionRecord;
use agent_core::token_meter::estimate_tokens;
use agent_core::ModelRequest;
use aria_agent_rsi::RsiLedger;
use async_trait::async_trait;

use crate::app::MakeModel;
use crate::store::{RerunRow, RsiStore};
use crate::util::now_ms;

/// Runs one model turn under a candidate playbook and records what it cost.
pub struct ModelRerunProbe {
    ledger: Arc<RsiLedger>,
    make_model: MakeModel,
    store: Arc<dyn RsiStore>,
    timeout: Duration,
}

impl ModelRerunProbe {
    pub fn new(
        ledger: Arc<RsiLedger>,
        make_model: MakeModel,
        store: Arc<dyn RsiStore>,
        timeout: Duration,
    ) -> Self {
        Self {
            ledger,
            make_model,
            store,
            // A zero deadline would fail every run; treat it as "one millisecond"
            // so a misconfiguration is visible instead of disabling the gate.
            timeout: timeout.max(Duration::from_millis(1)),
        }
    }

    /// The recorded task this proposal's evidence points at.
    ///
    /// Preference order: the exact turn named by the first evidence that carries
    /// one, then the first turn of any session that produced evidence for this
    /// tenant. `None` means "nothing to replay", which is inconclusive, never a
    /// pass.
    fn task_input(&self, proposal: &Proposal) -> Option<String> {
        let principal = proposal.scope.principal.as_str();
        let sessions = self.ledger.sessions(principal);
        if let Some(turn) = proposal
            .evidence
            .iter()
            .find_map(|evidence| evidence.turn_id.as_deref())
        {
            for session in &sessions {
                let records = self.ledger.records(principal, session);
                if let Some(input) = turn_input(&records, turn) {
                    return Some(input);
                }
            }
        }
        sessions
            .iter()
            .find_map(|session| first_turn_input(&self.ledger.records(principal, session)))
    }

    /// Persist the receipt. Failures are surfaced: an unaccounted model call must
    /// not silently unlock a promotion.
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        proposal: &Proposal,
        baseline: i32,
        outcome: &ValidationOutcome,
        prompt_tokens: u64,
        completion_tokens: u64,
        steps: u32,
        note: &str,
        started: Instant,
    ) -> SeamResult<()> {
        let score = match outcome {
            ValidationOutcome::Passed { score, .. }
            | ValidationOutcome::BelowThreshold { score, .. } => *score,
            ValidationOutcome::Inconclusive { .. } => baseline,
        };
        let row = RerunRow {
            id: uuid::Uuid::new_v4().to_string(),
            principal_id: proposal.scope.principal.clone(),
            domain: proposal.scope.domain.clone(),
            proposal_id: proposal.id.clone(),
            key: proposal.key(),
            baseline,
            score,
            verdict: outcome.as_str().to_string(),
            prompt_tokens,
            completion_tokens,
            steps,
            // Tool-free by design: see the module docs.
            tool_calls: 0,
            duration_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            note: note.to_string(),
            created_at_ms: now_ms(),
        };
        self.store
            .insert_rerun(&row)
            .await
            .map_err(|err| SeamError::Io(format!("re-run receipt could not be stored: {err:?}")))
    }
}

#[async_trait]
impl RerunProbe for ModelRerunProbe {
    async fn rerun(&self, proposal: &Proposal, baseline: i32) -> SeamResult<ValidationOutcome> {
        let started = Instant::now();
        let Some(input) = self.task_input(proposal) else {
            let outcome = ValidationOutcome::Inconclusive {
                reason: "no recorded task input for this proposal".into(),
            };
            self.record(proposal, baseline, &outcome, 0, 0, 0, "no_input", started)
                .await?;
            tracing::info!(
                proposal = %proposal.id,
                verdict = outcome.as_str(),
                "rsi: real re-run skipped (no input)"
            );
            return Ok(outcome);
        };

        let request = ModelRequest {
            system: system_prompt(proposal),
            context: String::new(),
            input,
        };
        let prompt_tokens = estimate_tokens(&request.system)
            + estimate_tokens(&request.context)
            + estimate_tokens(&request.input);
        let model = (self.make_model)();
        let call = tokio::time::timeout(self.timeout, model.complete(&request)).await;
        let (outcome, completion_tokens, note) = match call {
            Err(_) => (
                ValidationOutcome::Inconclusive {
                    reason: format!(
                        "re-run did not finish within {} ms",
                        self.timeout.as_millis()
                    ),
                },
                0,
                "timeout",
            ),
            Ok(Err(err)) => (
                ValidationOutcome::Inconclusive {
                    reason: err.to_string(),
                },
                0,
                "model_error",
            ),
            Ok(Ok(response)) if response.text.trim().is_empty() => (
                ValidationOutcome::BelowThreshold {
                    score: baseline,
                    baseline,
                    // The candidate promised `proposal.score`; it has to earn it.
                    required: proposal.score.max(baseline.saturating_add(1)),
                },
                estimate_tokens(&response.text),
                "empty_answer",
            ),
            Ok(Ok(response)) if proposal.score <= baseline => (
                ValidationOutcome::BelowThreshold {
                    score: proposal.score,
                    baseline,
                    required: baseline.saturating_add(1),
                },
                estimate_tokens(&response.text),
                "no_gain",
            ),
            Ok(Ok(response)) => (
                ValidationOutcome::Passed {
                    // Confirmation only: the gain still comes from replay.
                    score: proposal.score,
                    baseline,
                    replay: ReplayStats {
                        turns: 1,
                        tool_calls: 0,
                        tool_errors: 0,
                        denials: 0,
                    },
                },
                estimate_tokens(&response.text),
                "",
            ),
        };
        self.record(
            proposal,
            baseline,
            &outcome,
            prompt_tokens,
            completion_tokens,
            1,
            note,
            started,
        )
        .await?;
        tracing::info!(
            proposal = %proposal.id,
            verdict = outcome.as_str(),
            prompt_tokens,
            completion_tokens,
            elapsed_ms = started.elapsed().as_millis() as u64,
            "rsi: real re-run finished"
        );
        Ok(outcome)
    }
}

/// The candidate playbook, not the original system prompt: a re-run must not
/// pretend to reproduce a prompt that was never recorded.
fn system_prompt(proposal: &Proposal) -> String {
    format!(
        "You are re-running a recorded task under a candidate playbook.\n\
         Title: {title}\n\
         Pathology: {pathology}\n\
         --- playbook ---\n{body}\n--- end playbook ---\n\
         Apply the playbook and answer the task the way the agent should have.",
        title = proposal.title,
        pathology = proposal.pathology,
        body = proposal.body,
    )
}

fn turn_input(records: &[SessionRecord], turn: &str) -> Option<String> {
    records.iter().find_map(|record| match &record.event {
        SessionEvent::TurnStarted { turn_id, input } if turn_id == turn => Some(input.clone()),
        _ => None,
    })
}

fn first_turn_input(records: &[SessionRecord]) -> Option<String> {
    records.iter().find_map(|record| match &record.event {
        SessionEvent::TurnStarted { input, .. } => Some(input.clone()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use agent_core::seams::RsiScope;
    use agent_core::session::insight::Evidence;
    use agent_core::{CoreError, ModelClient, ModelResponse};
    use std::sync::Mutex;

    use crate::store::MemoryStore;

    struct StubModel {
        reply: Result<String, String>,
        delay: Duration,
        calls: Mutex<u32>,
    }

    impl StubModel {
        fn answering(text: &str) -> Self {
            Self {
                reply: Ok(text.to_string()),
                delay: Duration::ZERO,
                calls: Mutex::new(0),
            }
        }

        fn failing(message: &str) -> Self {
            Self {
                reply: Err(message.to_string()),
                delay: Duration::ZERO,
                calls: Mutex::new(0),
            }
        }

        fn slow(delay: Duration) -> Self {
            Self {
                reply: Ok("slow".into()),
                delay,
                calls: Mutex::new(0),
            }
        }

        fn calls(&self) -> u32 {
            *self.calls.lock().unwrap()
        }
    }

    #[async_trait]
    impl ModelClient for StubModel {
        async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            *self.calls.lock().unwrap() += 1;
            tokio::time::sleep(self.delay).await;
            self.reply
                .clone()
                .map(|text| ModelResponse { text })
                .map_err(CoreError::Model)
        }
    }

    fn proposal() -> Proposal {
        Proposal {
            id: "p1".into(),
            scope: RsiScope::new("tenant-a", "orders"),
            origin: RsiScope::new("tenant-a", "orders"),
            pathology: "retry_storm".into(),
            tool: Some("shell".into()),
            title: "Run the migrations once".into(),
            body: "Before calling shell, check the migration log.".into(),
            score: 70,
            baseline: 40,
            evidence: vec![Evidence {
                seq: 3,
                turn_id: Some("t1".into()),
                tool: Some("shell".into()),
                detail: "tool_error".into(),
            }],
            created_at_ms: 1,
            adaptation: None,
        }
    }

    fn record(seq: u64, event: SessionEvent) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: seq as i64,
            event,
        }
    }

    fn ledger_with(input: &str) -> Arc<RsiLedger> {
        let ledger = Arc::new(RsiLedger::with_capacity(64, 8, 1));
        ledger
            .append(
                "tenant-a",
                "s1",
                &[
                    // A non-turn record first: the scan must skip it, not stop.
                    record(
                        2,
                        SessionEvent::Step {
                            phase: "tool".into(),
                            label: None,
                        },
                    ),
                    record(
                        3,
                        SessionEvent::TurnStarted {
                            turn_id: "t1".into(),
                            input: input.into(),
                        },
                    ),
                ],
            )
            .unwrap();
        ledger
    }

    fn probe(
        ledger: Arc<RsiLedger>,
        model: Arc<StubModel>,
        store: Arc<MemoryStore>,
        timeout: Duration,
    ) -> ModelRerunProbe {
        ModelRerunProbe::new(
            ledger,
            Arc::new(move || Box::new(StubHandle(model.clone()))),
            store,
            timeout,
        )
    }

    /// `MakeModel` hands out fresh boxes; the test needs the same handle back.
    struct StubHandle(Arc<StubModel>);

    #[async_trait]
    impl ModelClient for StubHandle {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            self.0.complete(req).await
        }
    }

    #[tokio::test]
    async fn a_completed_re_run_confirms_the_replay_score_and_writes_a_receipt() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::answering("done"));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model.clone(), store.clone(), Duration::from_secs(5));

        let outcome = probe.rerun(&proposal(), 40).await.unwrap();
        assert_eq!(
            outcome,
            ValidationOutcome::Passed {
                score: 70,
                baseline: 40,
                replay: ReplayStats {
                    turns: 1,
                    tool_calls: 0,
                    tool_errors: 0,
                    denials: 0
                }
            }
        );
        assert_eq!(model.calls(), 1, "the probe must really call the model");
        let receipts = store.reruns();
        assert_eq!(receipts.len(), 1);
        let receipt = &receipts[0];
        assert_eq!(receipt.verdict, "passed");
        assert_eq!(receipt.score, 70);
        assert_eq!(receipt.steps, 1);
        assert_eq!(receipt.tool_calls, 0, "no side effects are recorded as 0");
        assert!(receipt.prompt_tokens > 0);
        assert!(receipt.completion_tokens > 0);
        assert!(receipt.note.is_empty());
        assert_eq!(receipt.key, "retry_storm:shell");
    }

    #[tokio::test]
    async fn an_empty_answer_is_a_failure_not_a_pass() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::answering("   "));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model, store.clone(), Duration::from_secs(5));

        let outcome = probe.rerun(&proposal(), 40).await.unwrap();
        assert_eq!(
            outcome,
            ValidationOutcome::BelowThreshold {
                score: 40,
                baseline: 40,
                required: 70
            }
        );
        assert_eq!(store.reruns()[0].verdict, "below_threshold");
        assert_eq!(store.reruns()[0].note, "empty_answer");
    }

    #[tokio::test]
    async fn a_candidate_without_a_gain_cannot_pass_on_a_clean_run() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::answering("done"));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model, store.clone(), Duration::from_secs(5));

        let mut candidate = proposal();
        candidate.score = 40;
        let outcome = probe.rerun(&candidate, 40).await.unwrap();
        assert_eq!(
            outcome,
            ValidationOutcome::BelowThreshold {
                score: 40,
                baseline: 40,
                required: 41
            }
        );
        assert_eq!(store.reruns()[0].note, "no_gain");
    }

    #[tokio::test]
    async fn evidence_without_a_turn_falls_back_to_the_first_recorded_turn() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::answering("done"));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model.clone(), store.clone(), Duration::from_secs(5));

        let mut candidate = proposal();
        candidate.evidence[0].turn_id = None;
        assert!(probe.rerun(&candidate, 40).await.unwrap().passed());
        assert_eq!(model.calls(), 1, "the fallback still replays a real turn");
        assert_eq!(store.reruns()[0].verdict, "passed");
    }

    #[tokio::test]
    async fn a_model_error_is_inconclusive_and_still_recorded() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::failing("upstream refused"));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model, store.clone(), Duration::from_secs(5));

        let outcome = probe.rerun(&proposal(), 40).await.unwrap();
        assert!(matches!(outcome, ValidationOutcome::Inconclusive { .. }));
        assert!(!outcome.passed());
        let receipts = store.reruns();
        assert_eq!(receipts[0].verdict, "inconclusive");
        assert_eq!(receipts[0].note, "model_error");
        assert_eq!(receipts[0].completion_tokens, 0);
        assert!(receipts[0].prompt_tokens > 0, "the attempt was paid for");
    }

    #[tokio::test]
    async fn a_timeout_is_inconclusive_and_records_the_deadline() {
        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::slow(Duration::from_millis(50)));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model, store.clone(), Duration::from_millis(1));

        let outcome = probe.rerun(&proposal(), 40).await.unwrap();
        match outcome {
            ValidationOutcome::Inconclusive { reason } => {
                assert!(reason.contains("ms"), "{reason}")
            }
            other => panic!("expected inconclusive, got {other:?}"),
        }
        assert_eq!(store.reruns()[0].note, "timeout");
    }

    #[tokio::test]
    async fn a_proposal_without_history_never_calls_the_model() {
        let ledger = Arc::new(RsiLedger::with_capacity(8, 4, 1));
        let model = Arc::new(StubModel::answering("unused"));
        let store = Arc::new(MemoryStore::new());
        let probe = probe(ledger, model.clone(), store.clone(), Duration::from_secs(5));

        let outcome = probe.rerun(&proposal(), 40).await.unwrap();
        assert!(!outcome.passed());
        assert_eq!(model.calls(), 0);
        let receipts = store.reruns();
        assert_eq!(receipts[0].steps, 0);
        assert_eq!(receipts[0].note, "no_input");
        assert_eq!(receipts[0].prompt_tokens, 0);
    }

    #[tokio::test]
    async fn a_failed_receipt_fails_closed() {
        struct RejectingStore;

        #[async_trait]
        impl RsiStore for RejectingStore {
            async fn insert_report(
                &self,
                _row: &crate::store::ReportRow,
            ) -> Result<(), crate::AppError> {
                Err(crate::AppError::Unavailable("down".into()))
            }
            async fn list_reports(
                &self,
                _principal: &crate::auth::Principal,
                _limit: i64,
            ) -> Result<Vec<crate::store::ReportRow>, crate::AppError> {
                Err(crate::AppError::Unavailable("down".into()))
            }
            async fn get_report(
                &self,
                _principal: &crate::auth::Principal,
                _id: &str,
            ) -> Result<crate::store::ReportRow, crate::AppError> {
                Err(crate::AppError::Unavailable("down".into()))
            }
            async fn insert_rerun(&self, _row: &RerunRow) -> Result<(), crate::AppError> {
                Err(crate::AppError::Unavailable("down".into()))
            }
        }

        let ledger = ledger_with("ship the order");
        let model = Arc::new(StubModel::answering("done"));
        let store: Arc<dyn RsiStore> = Arc::new(RejectingStore);
        // The stub must reject every write, not only receipts.
        let row = crate::store::ReportRow {
            id: "r1".into(),
            principal_id: "tenant-a".into(),
            session: "s1".into(),
            score: 0,
            truncated: false,
            findings: Vec::new(),
            created_at_ms: 0,
        };
        let principal = crate::auth::Principal::default_user();
        assert!(store.insert_report(&row).await.is_err());
        assert!(store.list_reports(&principal, 10).await.is_err());
        assert!(store.get_report(&principal, "r1").await.is_err());
        let probe = ModelRerunProbe::new(
            ledger,
            Arc::new(move || Box::new(StubHandle(model.clone()))),
            store,
            Duration::from_secs(5),
        );

        match probe.rerun(&proposal(), 40).await.unwrap_err() {
            SeamError::Io(message) => assert!(message.contains("receipt"), "{message}"),
            other => panic!("expected an io error, got {other:?}"),
        }
    }
}
