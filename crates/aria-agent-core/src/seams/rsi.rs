//! Recursive self-improvement seams: diagnosis, proposals, versioned playbooks.
//!
//! RSI turns a session's event ledger into reusable capability. Four steps, and
//! each one is a contract here rather than a hard-coded behaviour:
//!
//! 1. [`ReflectionEngine::analyze`] folds the ledger into an
//!    [`EfficiencyReport`](crate::session::insight::EfficiencyReport) — pure,
//!    deterministic, no model call.
//! 2. [`ReflectionEngine::propose`] maps diagnosed pathologies onto candidate
//!    improvements. Each candidate carries the [`RsiScope`] it was learned in.
//! 3. [`ReflectionEngine::validate`] replays the same history with the candidate
//!    applied. A candidate that does not beat the baseline is discarded.
//! 4. [`PlaybookStore::promote`] publishes a new **version**. It takes the
//!    revision the approver believes is current and fails closed on a mismatch,
//!    so two approvals can never silently overwrite each other.
//!
//! Three boundaries are deliberate:
//!
//! * **Human-in-the-loop by default.** A proposal is inert until promoted. An
//!   unanswered gate is a refusal, never an implicit allow (ADR-0016).
//! * **No hot swap.** Promotion produces a *versioned asset* read at the next
//!   turn; it never rewrites the system prompt of a running session (the removed
//!   Reef design, `requirements.md §7`).
//! * **Adaptation before reuse.** A playbook learned for one tenant is not
//!   portable as-is; a cross-tenant promotion without an [`Adaptation`] record is
//!   rejected with [`SeamError::Unadapted`].

use crate::seams::{SeamError, SeamResult};
use crate::session::insight::{EfficiencyReport, Evidence};
use crate::session::SessionRecord;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Deployment configuration for the RSI loop.
///
/// `enabled` defaults to `true`: the ledger records, a sweep can be requested and
/// `RsiState::disabled()` remains the explicit way to switch the feature off
/// (off means no ledger, no sweep, no timer and a single persistence sink, so the
/// turn path keeps its exact behaviour and cost). `real_rerun` stays off because
/// it is the only step that spends model calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RsiConfig {
    /// Master switch. On by default (ADR-0026); `RsiState::disabled()` is the
    /// explicit, tested way to turn the whole loop off.
    pub enabled: bool,
    /// Most recent ledger records folded into one report.
    pub max_records: usize,
    /// Most recent turns kept in one report.
    pub max_turns: usize,
    /// Supporting events kept per finding.
    pub max_evidence: usize,
    /// Failures of one tool that make it "repeatedly failing".
    pub repeated_failure: usize,
    /// Calls of one tool inside a turn that make it a retry storm.
    pub retry_storm: usize,
    /// Denials of one tool that make it a denial loop.
    pub denied_loop: usize,
    /// Compactions inside one turn that make it churn.
    pub compaction_churn: usize,
    /// Score improvement a proposal must show to be considered validated.
    pub min_score_gain: i32,
    /// Most proposals emitted for one report.
    pub max_proposals: usize,
    /// Publish validated proposals without asking a human. Trusted deployments
    /// only; the default keeps the gate closed.
    pub auto_promote: bool,
    /// Re-run the task for real to confirm a proposal (costs model calls and has
    /// side effects, so it is a separate explicit switch).
    pub real_rerun: bool,
    /// Deadline for one real re-run.
    pub real_rerun_timeout_ms: u64,
    /// Unhealthy turns accumulated before a sweep is due.
    pub sweep_threshold: usize,
}

impl Default for RsiConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_records: 2_000,
            max_turns: 50,
            max_evidence: 8,
            repeated_failure: 2,
            retry_storm: 3,
            denied_loop: 2,
            compaction_churn: 2,
            min_score_gain: 5,
            max_proposals: 4,
            auto_promote: false,
            real_rerun: false,
            real_rerun_timeout_ms: 600_000,
            sweep_threshold: 3,
        }
    }
}

impl RsiConfig {
    /// Reject configurations that would silently do nothing.
    pub fn validate(&self) -> SeamResult<()> {
        if self.max_records == 0
            || self.max_turns == 0
            || self.max_evidence == 0
            || self.repeated_failure == 0
            || self.retry_storm == 0
            || self.denied_loop == 0
            || self.compaction_churn == 0
        {
            return Err(SeamError::Invalid(
                "rsi limits must all be >= 1 (a zero cap would disable a check silently)".into(),
            ));
        }
        if self.min_score_gain < 1 {
            return Err(SeamError::Invalid(
                "rsi min_score_gain must be >= 1 (a zero gain accepts a no-op proposal)".into(),
            ));
        }
        if self.max_proposals == 0 {
            return Err(SeamError::Invalid("rsi max_proposals must be >= 1".into()));
        }
        if self.sweep_threshold == 0 {
            return Err(SeamError::Invalid(
                "rsi sweep_threshold must be >= 1".into(),
            ));
        }
        if self.real_rerun_timeout_ms == 0 {
            return Err(SeamError::Invalid(
                "rsi real_rerun_timeout_ms must be >= 1".into(),
            ));
        }
        Ok(())
    }
}

/// Where an improvement was learned, and the parameters it was learned with.
///
/// The parameter table is what makes adaptation possible at all: a rule that
/// says "approve above 500" is not transferable without knowing the threshold.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RsiScope {
    /// Tenant / principal the evidence came from.
    pub principal: String,
    /// Business domain inside that tenant.
    pub domain: String,
    /// Explicit knobs the playbook depends on (thresholds, field names, …).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub params: BTreeMap<String, String>,
}

impl RsiScope {
    pub fn new(principal: impl Into<String>, domain: impl Into<String>) -> Self {
        Self {
            principal: principal.into(),
            domain: domain.into(),
            params: BTreeMap::new(),
        }
    }

    pub fn with_param(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.params.insert(key.into(), value.into());
        self
    }

    /// True when both scopes address the same tenant and domain.
    pub fn same_placement(&self, other: &RsiScope) -> bool {
        self.principal == other.principal && self.domain == other.domain
    }

    pub fn validate(&self) -> SeamResult<()> {
        if self.principal.trim().is_empty() {
            return Err(SeamError::Invalid("rsi scope principal is empty".into()));
        }
        if self.domain.trim().is_empty() {
            return Err(SeamError::Invalid("rsi scope domain is empty".into()));
        }
        Ok(())
    }
}

/// Why a playbook may be reused in a *different* tenant, and what changed.
///
/// Required for every cross-tenant promotion: an unadapted copy is exactly the
/// failure mode the reference article warns about (customer A's thresholds
/// silently applied to customer B).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Adaptation {
    /// Tenant the playbook was learned in.
    pub from_principal: String,
    /// Domain it was learned in.
    pub from_domain: String,
    /// Tenant it is being reused in.
    pub to_principal: String,
    /// Domain it is being reused in.
    pub to_domain: String,
    /// Parameter names that were rewritten for the target placement.
    #[serde(default)]
    pub rewritten: Vec<String>,
    /// Why those rewrites are the right ones.
    pub rationale: String,
}

impl Adaptation {
    /// Validate the record *against the target scope*: a record that names a
    /// different destination than the promotion target is a mismatch, not an
    /// adaptation.
    pub fn validate_for(&self, target: &RsiScope) -> SeamResult<()> {
        if self.rewritten.is_empty() {
            return Err(SeamError::Unadapted(
                "cross-tenant promotion must list the rewritten parameters".into(),
            ));
        }
        if self.rationale.trim().is_empty() {
            return Err(SeamError::Unadapted(
                "cross-tenant promotion must record why it is valid".into(),
            ));
        }
        if self.to_principal != target.principal || self.to_domain != target.domain {
            return Err(SeamError::Unadapted(format!(
                "adaptation targets {}/{} but promotion targets {}/{}",
                self.to_principal, self.to_domain, target.principal, target.domain
            )));
        }
        Ok(())
    }

    /// The placement the playbook came from.
    pub fn source_scope(&self) -> RsiScope {
        RsiScope::new(self.from_principal.clone(), self.from_domain.clone())
    }
}

/// A candidate improvement, inert until promoted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub id: String,
    /// Placement this proposal would take effect in.
    pub scope: RsiScope,
    /// Placement the evidence came from. Equal to `scope` unless the proposal is
    /// an adapted cross-tenant copy.
    #[serde(default)]
    pub origin: RsiScope,
    /// Pathology key this proposal addresses (see
    /// [`Finding::key`](crate::session::insight::Finding::key)).
    pub pathology: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    pub title: String,
    pub body: String,
    /// Score the history would have if this proposal were in force.
    pub score: i32,
    /// Score the history actually had.
    pub baseline: i32,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptation: Option<Adaptation>,
}

impl Proposal {
    /// Stable key a promoted playbook is versioned under.
    pub fn key(&self) -> String {
        match &self.tool {
            Some(tool) => format!("{}:{tool}", self.pathology),
            None => self.pathology.clone(),
        }
    }
}

/// A promoted, versioned playbook.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Playbook {
    pub key: String,
    /// Placement the playbook takes effect in.
    pub scope: RsiScope,
    /// Placement the evidence came from (differs from `scope` only for an
    /// adapted cross-tenant promotion).
    #[serde(default)]
    pub origin: RsiScope,
    /// Monotonic, starting at 1.
    pub revision: u64,
    pub title: String,
    pub body: String,
    /// Validated score of the history the proposal was derived from.
    pub score: i32,
    /// Proposal that produced this revision.
    pub source_proposal: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptation: Option<Adaptation>,
    pub updated_at_ms: i64,
}

impl Playbook {
    /// Prompt section text: the playbook body plus its version stamp, so a
    /// reader can always tell which revision produced an answer.
    pub fn as_prompt_section(&self) -> String {
        format!(
            "## Playbook {} (r{})\n\n{}\n",
            self.key, self.revision, self.body
        )
    }
}

/// What the engine asks the store to hold while a human decides.
///
/// `origin` and `scope` are separate on purpose: a candidate learned in tenant A
/// but published for tenant B needs an [`Adaptation`], and one learned and
/// published in the same placement must not pretend to have one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookCandidate {
    pub proposal_id: String,
    /// Placement the evidence came from.
    pub origin: RsiScope,
    /// Placement the playbook will take effect in.
    pub scope: RsiScope,
    pub key: String,
    pub title: String,
    pub body: String,
    pub score: i32,
    pub baseline: i32,
    /// Events that justified the proposal (kept for audit).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<Evidence>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptation: Option<Adaptation>,
}

impl PlaybookCandidate {
    /// Reject anything that would publish an unverifiable or unadapted playbook.
    pub fn validate(&self) -> SeamResult<()> {
        self.origin.validate()?;
        self.scope.validate()?;
        if self.key.trim().is_empty() {
            return Err(SeamError::Invalid("playbook key is empty".into()));
        }
        if self.body.trim().is_empty() {
            return Err(SeamError::Invalid("playbook body is empty".into()));
        }
        if self.score <= self.baseline {
            return Err(SeamError::Invalid(format!(
                "playbook does not improve the baseline ({} <= {})",
                self.score, self.baseline
            )));
        }
        if self.origin.same_placement(&self.scope) {
            return Ok(());
        }
        // Cross-tenant: an adaptation record is mandatory, and it must actually
        // be about this origin and this destination.
        let adaptation = self.adaptation.as_ref().ok_or_else(|| {
            SeamError::Unadapted(format!(
                "playbook learned in {}/{} cannot be published to {}/{} without an adaptation record",
                self.origin.principal, self.origin.domain, self.scope.principal, self.scope.domain
            ))
        })?;
        adaptation.validate_for(&self.scope)?;
        if !adaptation.source_scope().same_placement(&self.origin) {
            return Err(SeamError::Unadapted(format!(
                "adaptation source {}/{} does not match the evidence placement {}/{}",
                adaptation.from_principal,
                adaptation.from_domain,
                self.origin.principal,
                self.origin.domain
            )));
        }
        Ok(())
    }
}

/// Replay statistics behind a validation verdict.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayStats {
    /// Turns replayed.
    pub turns: usize,
    /// Tool calls replayed.
    pub tool_calls: usize,
    /// Tool calls that failed in the replayed history.
    pub tool_errors: usize,
    /// Approvals denied in the replayed history.
    pub denials: usize,
}

/// Verdict of the validation gate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ValidationOutcome {
    /// The candidate beats the baseline by at least the required gain.
    Passed {
        score: i32,
        baseline: i32,
        /// Present for the offline replay; the real re-run reports its own.
        #[serde(default)]
        replay: ReplayStats,
    },
    /// The candidate did not earn its place and must be discarded.
    BelowThreshold {
        score: i32,
        baseline: i32,
        required: i32,
    },
    /// Validation could not run (e.g. no history). Never treated as a pass.
    Inconclusive { reason: String },
}

impl ValidationOutcome {
    pub fn passed(&self) -> bool {
        matches!(self, ValidationOutcome::Passed { .. })
    }

    /// Stable machine-readable label.
    pub fn as_str(&self) -> &'static str {
        match self {
            ValidationOutcome::Passed { .. } => "passed",
            ValidationOutcome::BelowThreshold { .. } => "below_threshold",
            ValidationOutcome::Inconclusive { .. } => "inconclusive",
        }
    }
}

/// Ledger diagnosis + proposal generation + validation.
#[async_trait]
pub trait ReflectionEngine: Send + Sync {
    /// Fold one session ledger into a report.
    async fn analyze(
        &self,
        session: &str,
        records: &[SessionRecord],
    ) -> SeamResult<EfficiencyReport>;

    /// Turn a report into inert candidates for the placement in `scope`.
    ///
    /// The scope is an argument rather than engine state so one engine instance
    /// can serve every tenant without ever mixing up whose evidence is whose.
    async fn propose(
        &self,
        report: &EfficiencyReport,
        scope: &RsiScope,
    ) -> SeamResult<Vec<Proposal>>;

    /// Replay the report's history with `proposal` applied.
    async fn validate(
        &self,
        proposal: &Proposal,
        report: &EfficiencyReport,
    ) -> SeamResult<ValidationOutcome>;
}

/// Optional real re-run used as a second, higher-confidence gate.
///
/// Kept behind its own trait because it is the only RSI step with external cost
/// and side effects; the offline replay must stay sufficient on its own.
#[async_trait]
pub trait RerunProbe: Send + Sync {
    async fn rerun(&self, proposal: &Proposal, baseline: i32) -> SeamResult<ValidationOutcome>;
}

/// Versioned playbook storage with a revision guard.
#[async_trait]
pub trait PlaybookStore: Send + Sync {
    /// Hold a validated candidate as pending. Must not publish it.
    async fn submit(&self, candidate: PlaybookCandidate) -> SeamResult<Proposal>;

    /// Pending proposals for one scope, oldest first.
    async fn pending(&self, scope: &RsiScope) -> SeamResult<Vec<Proposal>>;

    /// Publish `proposal_id` as the next revision.
    ///
    /// `expected_revision` is the revision the approver believes is current
    /// (`0` when none exists). A mismatch is [`SeamError::Conflict`]; it is never
    /// resolved by last-writer-wins (the pattern ADR-0021 established for egress
    /// policy revisions).
    async fn promote(&self, proposal_id: &str, expected_revision: u64) -> SeamResult<Playbook>;

    /// Drop a pending proposal without publishing anything.
    async fn discard(&self, proposal_id: &str) -> SeamResult<Proposal>;

    /// Current revision, `None` when the key was never promoted.
    async fn current(&self, scope: &RsiScope, key: &str) -> SeamResult<Option<Playbook>>;

    /// Every revision of one key, oldest first (audit trail).
    async fn list_versions(&self, scope: &RsiScope, key: &str) -> SeamResult<Vec<Playbook>>;
}

/// Fail-closed reflection: no engine configured means "nothing to diagnose".
///
/// `analyze` still succeeds with an empty report so a caller that only reads a
/// report is not forced to special-case a missing engine, while `propose` and
/// `validate` refuse instead of inventing an improvement.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoReflection;

#[async_trait]
impl ReflectionEngine for NoReflection {
    async fn analyze(
        &self,
        session: &str,
        _records: &[SessionRecord],
    ) -> SeamResult<EfficiencyReport> {
        Ok(EfficiencyReport {
            session: session.to_string(),
            folded_records: 0,
            truncated: false,
            turns: Vec::new(),
            findings: Vec::new(),
            score: 100,
        })
    }

    async fn propose(
        &self,
        _report: &EfficiencyReport,
        _scope: &RsiScope,
    ) -> SeamResult<Vec<Proposal>> {
        Err(SeamError::Unavailable(
            "no reflection engine is configured; RSI proposals are unavailable".into(),
        ))
    }

    async fn validate(
        &self,
        _proposal: &Proposal,
        _report: &EfficiencyReport,
    ) -> SeamResult<ValidationOutcome> {
        Err(SeamError::Unavailable(
            "no reflection engine is configured; RSI validation is unavailable".into(),
        ))
    }
}

/// Fail-closed store: no playbook provider means no version can be published.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPlaybooks;

fn no_store<T>() -> SeamResult<T> {
    Err(SeamError::Unavailable(
        "no playbook store is configured; RSI cannot hold or publish playbooks".into(),
    ))
}

#[async_trait]
impl PlaybookStore for NoPlaybooks {
    async fn submit(&self, _candidate: PlaybookCandidate) -> SeamResult<Proposal> {
        no_store()
    }

    async fn pending(&self, _scope: &RsiScope) -> SeamResult<Vec<Proposal>> {
        no_store()
    }

    async fn promote(&self, _id: &str, _expected: u64) -> SeamResult<Playbook> {
        no_store()
    }

    async fn discard(&self, _id: &str) -> SeamResult<Proposal> {
        no_store()
    }

    async fn current(&self, _scope: &RsiScope, _key: &str) -> SeamResult<Option<Playbook>> {
        no_store()
    }

    async fn list_versions(&self, _scope: &RsiScope, _key: &str) -> SeamResult<Vec<Playbook>> {
        no_store()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate() -> PlaybookCandidate {
        PlaybookCandidate {
            proposal_id: "p1".into(),
            origin: RsiScope::new("tenant", "orders"),
            scope: RsiScope::new("tenant", "orders"),
            key: "retry_storm:shell".into(),
            title: "Check before retrying".into(),
            body: "1. verify the shell tool is reachable".into(),
            score: 92,
            baseline: 70,
            evidence: Vec::new(),
            adaptation: None,
        }
    }

    fn adaptation_for(origin: &RsiScope, target: &RsiScope) -> Adaptation {
        Adaptation {
            from_principal: origin.principal.clone(),
            from_domain: origin.domain.clone(),
            to_principal: target.principal.clone(),
            to_domain: target.domain.clone(),
            rewritten: vec!["approval_threshold".into()],
            rationale: "target approves above 1000".into(),
        }
    }

    #[test]
    fn the_default_configuration_is_on_but_never_spends_a_model_call() {
        let config = RsiConfig::default();
        assert!(config.enabled, "the ledger and diagnosis are on by default");
        assert!(!config.auto_promote, "publishing still needs a human");
        assert!(!config.real_rerun, "a real re-run costs model calls");
        assert!(config.validate().is_ok());
        let json = serde_json::to_value(&config).unwrap();
        assert_eq!(json["enabled"], true);
        // Missing fields fall back to the documented defaults, not to zero.
        let partial: RsiConfig = serde_json::from_value(serde_json::json!({"enabled": false}))
            .expect("partial config deserializes");
        assert!(!partial.enabled);
        assert_eq!(partial.max_records, 2_000);
        assert_eq!(partial.min_score_gain, 5);
    }

    #[test]
    fn zero_limits_are_rejected_instead_of_silently_disabling_a_check() {
        for mutate in [
            (|c: &mut RsiConfig| c.max_records = 0) as fn(&mut RsiConfig),
            |c| c.max_turns = 0,
            |c| c.max_evidence = 0,
            |c| c.repeated_failure = 0,
            |c| c.retry_storm = 0,
            |c| c.denied_loop = 0,
            |c| c.compaction_churn = 0,
            |c| c.max_proposals = 0,
            |c| c.sweep_threshold = 0,
            |c| c.real_rerun_timeout_ms = 0,
        ] {
            let mut config = RsiConfig::default();
            mutate(&mut config);
            assert!(config.validate().is_err(), "{config:?}");
        }
        let config = RsiConfig {
            min_score_gain: 0,
            ..RsiConfig::default()
        };
        assert!(matches!(config.validate(), Err(SeamError::Invalid(_))));
    }

    #[test]
    fn scopes_require_a_placement_and_carry_parameters() {
        let scope = RsiScope::new("t1", "orders").with_param("approval_threshold", "500");
        scope.validate().unwrap();
        assert_eq!(scope.params.get("approval_threshold").unwrap(), "500");
        assert!(scope.same_placement(&RsiScope::new("t1", "orders")));
        assert!(!scope.same_placement(&RsiScope::new("t2", "orders")));
        assert!(RsiScope::new("", "orders").validate().is_err());
        assert!(RsiScope::new("t1", " ").validate().is_err());
    }

    #[test]
    fn cross_tenant_adaptation_must_be_complete_and_on_target() {
        let target = RsiScope::new("t2", "orders");
        let good = Adaptation {
            from_principal: "t1".into(),
            from_domain: "orders".into(),
            to_principal: "t2".into(),
            to_domain: "orders".into(),
            rewritten: vec!["approval_threshold".into()],
            rationale: "customer B approves above 1000".into(),
        };
        good.validate_for(&target).unwrap();
        assert_eq!(good.source_scope(), RsiScope::new("t1", "orders"));

        let mut missing_rewrites = good.clone();
        missing_rewrites.rewritten.clear();
        assert!(matches!(
            missing_rewrites.validate_for(&target),
            Err(SeamError::Unadapted(_))
        ));

        let mut no_reason = good.clone();
        no_reason.rationale = "  ".into();
        assert!(matches!(
            no_reason.validate_for(&target),
            Err(SeamError::Unadapted(_))
        ));

        let mut wrong_target = good;
        wrong_target.to_principal = "t3".into();
        assert!(matches!(
            wrong_target.validate_for(&target),
            Err(SeamError::Unadapted(_))
        ));
    }

    #[test]
    fn candidates_must_beat_the_baseline() {
        candidate().validate().unwrap();

        let mut no_gain = candidate();
        no_gain.score = no_gain.baseline;
        assert!(matches!(no_gain.validate(), Err(SeamError::Invalid(_))));

        let mut empty_key = candidate();
        empty_key.key = " ".into();
        assert!(matches!(empty_key.validate(), Err(SeamError::Invalid(_))));

        let mut empty_body = candidate();
        empty_body.body = String::new();
        assert!(matches!(empty_body.validate(), Err(SeamError::Invalid(_))));

        let mut bad_scope = candidate();
        bad_scope.scope = RsiScope::new("", "orders");
        assert!(matches!(bad_scope.validate(), Err(SeamError::Invalid(_))));

        let mut bad_origin = candidate();
        bad_origin.origin = RsiScope::new("", "orders");
        assert!(matches!(bad_origin.validate(), Err(SeamError::Invalid(_))));
    }

    #[test]
    fn cross_tenant_candidates_need_a_matching_adaptation() {
        let origin = RsiScope::new("tenant-a", "orders");
        let target = RsiScope::new("tenant-b", "orders");

        let mut portable = candidate();
        portable.origin = origin.clone();
        portable.scope = target.clone();
        assert!(matches!(portable.validate(), Err(SeamError::Unadapted(_))));

        portable.adaptation = Some(adaptation_for(&origin, &target));
        portable.validate().unwrap();

        // An adaptation that names another destination is not an adaptation.
        let mut wrong = portable.clone();
        wrong.adaptation = Some(adaptation_for(
            &origin,
            &RsiScope::new("tenant-c", "orders"),
        ));
        assert!(matches!(wrong.validate(), Err(SeamError::Unadapted(_))));

        // …and one that came from a different placement than the evidence is
        // equally useless.
        let mut wrong_source = portable.clone();
        wrong_source.adaptation = Some(adaptation_for(
            &RsiScope::new("tenant-x", "orders"),
            &target,
        ));
        assert!(matches!(
            wrong_source.validate(),
            Err(SeamError::Unadapted(_))
        ));

        // Same placement with a stray adaptation record is still fine: the
        // record simply does not apply.
        let mut local = candidate();
        local.adaptation = Some(adaptation_for(&origin, &target));
        local.validate().unwrap();
    }

    #[test]
    fn proposal_keys_and_outcome_labels_are_stable() {
        let proposal = Proposal {
            id: "p".into(),
            scope: RsiScope::new("t", "d"),
            origin: RsiScope::new("t", "d"),
            pathology: "retry_storm".into(),
            tool: Some("shell".into()),
            title: "t".into(),
            body: "b".into(),
            score: 90,
            baseline: 70,
            evidence: Vec::new(),
            created_at_ms: 0,
            adaptation: None,
        };
        assert_eq!(proposal.key(), "retry_storm:shell");
        let mut unscoped = proposal.clone();
        unscoped.tool = None;
        assert_eq!(unscoped.key(), "retry_storm");

        let passed = ValidationOutcome::Passed {
            score: 90,
            baseline: 70,
            replay: ReplayStats::default(),
        };
        assert!(passed.passed());
        assert_eq!(passed.as_str(), "passed");
        let below = ValidationOutcome::BelowThreshold {
            score: 71,
            baseline: 70,
            required: 5,
        };
        assert!(!below.passed());
        assert_eq!(below.as_str(), "below_threshold");
        assert!(!ValidationOutcome::Inconclusive {
            reason: "no history".into()
        }
        .passed());
        assert_eq!(
            ValidationOutcome::Inconclusive {
                reason: "no history".into()
            }
            .as_str(),
            "inconclusive"
        );
        let json = serde_json::to_value(ValidationOutcome::Passed {
            score: 90,
            baseline: 70,
            replay: ReplayStats {
                turns: 1,
                tool_calls: 2,
                tool_errors: 1,
                denials: 0,
            },
        })
        .unwrap();
        assert_eq!(json["outcome"], "passed");
        let back: ValidationOutcome = serde_json::from_value(json).unwrap();
        assert!(back.passed());
    }

    #[test]
    fn a_playbook_renders_a_versioned_prompt_section() {
        let playbook = Playbook {
            key: "retry_storm:shell".into(),
            scope: RsiScope::new("t", "d"),
            origin: RsiScope::new("t", "d"),
            revision: 3,
            title: "Check before retrying".into(),
            body: "step one".into(),
            score: 92,
            source_proposal: "p1".into(),
            adaptation: None,
            updated_at_ms: 1,
        };
        let section = playbook.as_prompt_section();
        assert!(section.contains("retry_storm:shell"));
        assert!(section.contains("r3"));
        assert!(section.contains("step one"));
        let json = serde_json::to_string(&playbook).unwrap();
        let back: Playbook = serde_json::from_str(&json).unwrap();
        assert_eq!(back, playbook);
    }

    #[tokio::test]
    async fn the_fail_closed_defaults_refuse_to_invent_or_publish() {
        let engine = NoReflection;
        let report = engine.analyze("s", &[]).await.unwrap();
        assert!(report.findings.is_empty());
        assert_eq!(report.score, 100);
        assert!(matches!(
            engine.propose(&report, &RsiScope::new("t", "d")).await,
            Err(SeamError::Unavailable(_))
        ));
        let proposal = Proposal {
            id: "p".into(),
            scope: RsiScope::new("t", "d"),
            origin: RsiScope::new("t", "d"),
            pathology: "retry_storm".into(),
            tool: None,
            title: "t".into(),
            body: "b".into(),
            score: 90,
            baseline: 70,
            evidence: Vec::new(),
            created_at_ms: 0,
            adaptation: None,
        };
        assert!(matches!(
            engine.validate(&proposal, &report).await,
            Err(SeamError::Unavailable(_))
        ));

        let store = NoPlaybooks;
        let scope = RsiScope::new("t", "d");
        assert!(matches!(
            store.submit(candidate()).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            store.pending(&scope).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            store.promote("p1", 0).await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            store.discard("p1").await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            store.current(&scope, "k").await,
            Err(SeamError::Unavailable(_))
        ));
        assert!(matches!(
            store.list_versions(&scope, "k").await,
            Err(SeamError::Unavailable(_))
        ));
    }
}
