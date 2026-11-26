//! Offline replay validation: the first gate an improvement must pass.
//!
//! "Verify before you keep it" is the load-bearing rule of the whole mechanism.
//! This validator never calls a model and never runs code: it re-scores the very
//! history that produced the finding, with the targeted pathology removed, using
//! the *same* scoring function as the diagnosis
//! ([`health_score`](agent_core::session::insight::health_score)). Three
//! consequences follow, and all three are intended:
//!
//! * the replay is free, so it can run inside a sweep;
//! * it is falsifiable — a proposal that does not remove a real pathology scores
//!   exactly the same and is rejected;
//! * it is conservative — it claims only "this specific pattern would not have
//!   cost you these points", never "the task would have succeeded".
//!
//! Real re-execution is a *second*, optional gate
//! ([`RerunProbe`](agent_core::seams::RerunProbe)) because it costs model calls
//! and can have side effects.

use agent_core::seams::{Proposal, ReplayStats, ValidationOutcome};
use agent_core::session::insight::{health_score, EfficiencyReport};

/// How much better than the baseline a proposal must score.
#[derive(Debug, Clone, Copy)]
pub struct ReplayValidator {
    min_score_gain: i32,
}

impl ReplayValidator {
    pub fn new(min_score_gain: i32) -> Self {
        Self {
            min_score_gain: min_score_gain.max(1),
        }
    }

    pub fn min_score_gain(&self) -> i32 {
        self.min_score_gain
    }

    /// Replay one proposal against the report it came from.
    pub fn validate(&self, proposal: &Proposal, report: &EfficiencyReport) -> ValidationOutcome {
        if report.turns.is_empty() {
            return ValidationOutcome::Inconclusive {
                reason: "the ledger holds no completed turn to replay".into(),
            };
        }
        if proposal.baseline != report.score {
            // The proposal was made against a different history; replaying it
            // now would validate a claim nobody is making.
            return ValidationOutcome::Inconclusive {
                reason: format!(
                    "proposal baseline {} does not match the report score {}",
                    proposal.baseline, report.score
                ),
            };
        }
        let (score, replay) = replay_score(proposal, report);
        if score - report.score >= self.min_score_gain {
            ValidationOutcome::Passed {
                score,
                baseline: report.score,
                replay,
            }
        } else {
            ValidationOutcome::BelowThreshold {
                score,
                baseline: report.score,
                required: self.min_score_gain,
            }
        }
    }
}

/// Projected score and the metrics the projection was built from.
pub fn replay_score(proposal: &Proposal, report: &EfficiencyReport) -> (i32, ReplayStats) {
    let mut findings = Vec::with_capacity(report.findings.len());
    for finding in &report.findings {
        let addressed =
            finding.pathology.as_str() == proposal.pathology && finding.tool == proposal.tool;
        if !addressed {
            findings.push(finding.clone());
        }
    }
    let replay = ReplayStats {
        turns: report.turns.len(),
        tool_calls: report.turns.iter().map(|t| t.tool_calls).sum(),
        tool_errors: report.turns.iter().map(|t| t.tool_errors).sum(),
        denials: report.turns.iter().map(|t| t.approvals_denied).sum(),
    };
    (health_score(&findings, &report.turns), replay)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::session::insight::{analyze, Finding, InsightLimits, Pathology, TurnInsight};
    use agent_core::session::projection::TurnStatus;
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
                latency_ms: None,
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

    fn proposal_for(report: &EfficiencyReport, pathology: &str, tool: Option<&str>) -> Proposal {
        Proposal {
            id: "p".into(),
            scope: agent_core::seams::RsiScope::new("t", "d"),
            origin: agent_core::seams::RsiScope::new("t", "d"),
            pathology: pathology.into(),
            tool: tool.map(str::to_string),
            title: "t".into(),
            body: "b".into(),
            score: 0,
            baseline: report.score,
            evidence: Vec::new(),
            created_at_ms: 0,
            adaptation: None,
        }
    }

    #[test]
    fn a_targeted_proposal_passes_and_reports_its_replay_stats() {
        let report = analyze("s1", &failing_ledger(), &InsightLimits::default());
        let validator = ReplayValidator::new(5);
        let outcome = validator.validate(
            &proposal_for(&report, "repeated_tool_failure", Some("shell")),
            &report,
        );
        assert!(outcome.passed(), "{outcome:?}");
        if let ValidationOutcome::Passed {
            score,
            baseline,
            replay,
        } = &outcome
        {
            assert!(score > baseline);
            assert_eq!(replay.turns, 1);
            assert_eq!(replay.tool_calls, 2);
            assert_eq!(replay.tool_errors, 2);
            assert_eq!(replay.denials, 0);
        }
        assert_eq!(validator.min_score_gain(), 5);
    }

    #[test]
    fn a_proposal_for_the_wrong_tool_cannot_pass() {
        let report = analyze("s1", &failing_ledger(), &InsightLimits::default());
        let outcome = ReplayValidator::new(5).validate(
            &proposal_for(&report, "repeated_tool_failure", Some("fs_read")),
            &report,
        );
        assert!(matches!(outcome, ValidationOutcome::BelowThreshold { .. }));
        assert_eq!(outcome.as_str(), "below_threshold");
    }

    #[test]
    fn an_impossible_gain_is_below_threshold() {
        let report = analyze("s1", &failing_ledger(), &InsightLimits::default());
        let outcome = ReplayValidator::new(1_000).validate(
            &proposal_for(&report, "repeated_tool_failure", Some("shell")),
            &report,
        );
        assert_eq!(outcome.as_str(), "below_threshold");
        if let ValidationOutcome::BelowThreshold {
            score,
            baseline,
            required,
        } = &outcome
        {
            assert!(score > baseline);
            assert_eq!(*required, 1_000);
        }
    }

    #[test]
    fn a_stale_baseline_is_inconclusive_not_a_pass() {
        let report = analyze("s1", &failing_ledger(), &InsightLimits::default());
        let mut proposal = proposal_for(&report, "repeated_tool_failure", Some("shell"));
        proposal.baseline = report.score + 1;
        let outcome = ReplayValidator::new(1).validate(&proposal, &report);
        assert!(matches!(outcome, ValidationOutcome::Inconclusive { .. }));

        let empty = EfficiencyReport {
            session: "s".into(),
            folded_records: 0,
            truncated: false,
            turns: Vec::new(),
            findings: Vec::new(),
            score: 100,
        };
        assert!(matches!(
            ReplayValidator::new(1).validate(&proposal_for(&empty, "retry_storm", None), &empty),
            ValidationOutcome::Inconclusive { .. }
        ));
    }

    #[test]
    fn the_validator_clamps_a_zero_gain_requirement() {
        // Zero would accept a proposal that changes nothing; the constructor
        // raises it to the smallest falsifiable gain.
        assert_eq!(ReplayValidator::new(0).min_score_gain(), 1);
        assert_eq!(ReplayValidator::new(-3).min_score_gain(), 1);
    }

    #[test]
    fn replay_only_removes_the_targeted_finding() {
        let report = EfficiencyReport {
            session: "s".into(),
            folded_records: 1,
            truncated: false,
            turns: vec![TurnInsight {
                turn_id: "t1".into(),
                status: TurnStatus::Completed,
                steps: 1,
                tool_calls: 1,
                tool_errors: 1,
                approvals_denied: 1,
                compactions: 0,
                measured_tools: 0,
                latency_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
            }],
            findings: vec![
                Finding {
                    pathology: Pathology::RepeatedToolFailure,
                    tool: Some("shell".into()),
                    occurrences: 2,
                    evidence: Vec::new(),
                },
                Finding {
                    pathology: Pathology::ApprovalDeniedLoop,
                    tool: Some("shell".into()),
                    occurrences: 2,
                    evidence: Vec::new(),
                },
            ],
            score: 40,
        };
        let (score, _) = replay_score(
            &proposal_for(&report, "repeated_tool_failure", Some("shell")),
            &report,
        );
        // Only one finding was removed, so the remaining one still costs points.
        assert!(score > 40 && score < 100);
    }
}
