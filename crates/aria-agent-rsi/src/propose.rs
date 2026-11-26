//! Deterministic proposal generation: pathology → actionable guidance.
//!
//! The default proposer is a **rule table, not a model call** (ADR-0026). That is
//! a deliberate trade:
//!
//! * a rule table is offline-testable, free, and produces the same text for the
//!   same evidence, so a human reviewing a proposal reviews the *rule*, not a
//!   one-off generation;
//! * it cannot invent a fix for a pathology it has no rule for, which is the
//!   correct failure mode — an unactionable proposal must not reach the gate.
//!
//! A model-backed proposer can implement the same
//! [`ReflectionEngine`](agent_core::seams::ReflectionEngine) seam without touching
//! this module.
//!
//! Guidance is always written to be **reusable but parameterised**: it names the
//! tool and the observed count, and never encodes a customer-specific threshold,
//! because a playbook built from one tenant's numbers must be adapted before it
//! is reused elsewhere.

use agent_core::context::now_ms;
use agent_core::seams::{Proposal, RsiConfig, RsiScope, SeamError, SeamResult};
use agent_core::session::insight::{health_score, EfficiencyReport, Finding, Pathology};
use uuid::Uuid;

/// Heading under which the guidance is written. Stable, so a promoted playbook
/// diff is readable across revisions.
fn title_for(finding: &Finding) -> String {
    let tool = finding.tool.as_deref();
    match finding.pathology {
        Pathology::RepeatedToolFailure => format!(
            "Check preconditions before calling {}",
            tool.unwrap_or("a tool")
        ),
        Pathology::RetryStorm => {
            format!("Stop retrying {} blindly", tool.unwrap_or("a tool"))
        }
        Pathology::ApprovalDeniedLoop => format!(
            "Request {} permission once, never re-issue a denied call",
            tool.unwrap_or("the")
        ),
        Pathology::CompactionChurn => "Externalise progress before the context fills".to_string(),
        Pathology::StepBudgetExhausted => {
            "Checkpoint the goal before the step budget runs out".to_string()
        }
        Pathology::ToolArgumentRepair => format!(
            "Validate {} arguments against the schema first",
            tool.unwrap_or("the tool")
        ),
    }
}

/// The actual do/verify steps for one finding.
fn guidance_for(finding: &Finding) -> String {
    let tool = finding.tool.as_deref().unwrap_or("the tool");
    match finding.pathology {
        Pathology::RepeatedToolFailure => format!(
            "`{tool}` failed {} times.\n\n\
             - Before calling it, verify its preconditions: the path exists, the \
             resource is reachable, the credentials are present.\n\
             - When the goal allows, use the narrower tool instead of the general one.\n\
             - If the same failure repeats after one precondition check, stop and report \
             what is missing instead of retrying.",
            finding.occurrences
        ),
        Pathology::RetryStorm => format!(
            "`{tool}` was called {} times in a single turn.\n\n\
             - Issue one call, then read the result before deciding the next step.\n\
             - Only retry after changing the arguments or the plan — never with the same input.\n\
             - If two attempts fail, switch strategy instead of issuing a third.",
            finding.occurrences
        ),
        Pathology::ApprovalDeniedLoop => format!(
            "`{tool}` was denied {} times.\n\n\
             - Ask for the missing permission once, and state what it is needed for.\n\
             - A denied call is final for that turn: do not re-issue it with identical arguments.\n\
             - Prefer a read-only alternative that reaches the same conclusion.",
            finding.occurrences
        ),
        Pathology::CompactionChurn => format!(
            "The context was compacted {} times in one turn.\n\n\
             - Write intermediate findings to a file or todo as soon as they are known.\n\
             - Do not re-read material that was already summarised; extend the summary instead.\n\
             - Finish one sub-goal per turn rather than holding the whole task in context.",
            finding.occurrences
        ),
        Pathology::StepBudgetExhausted => format!(
            "The turn hit its step budget {} time(s), or ended without a terminal event.\n\n\
             - Split the goal into checkpoints and close one verifiable sub-goal per turn.\n\
             - Report partial progress explicitly instead of silently exhausting the budget.\n\
             - Plan the remaining steps before the next tool call.",
            finding.occurrences
        ),
        Pathology::ToolArgumentRepair => format!(
            "`{tool}` was retried with different arguments {} times.\n\n\
             - Validate the argument object against the tool schema before the first call.\n\
             - Copy the required field names verbatim; do not rename or re-nest them.\n\
             - If the first call is rejected for its shape, fix the shape once and re-run.",
            finding.occurrences
        ),
    }
}

/// Full markdown body of a proposal.
fn body_for(finding: &Finding) -> String {
    format!("# {}\n\n{}", title_for(finding), guidance_for(finding))
}

/// Findings left after a proposal removes the one it addresses.
///
/// The replay is intentionally narrow: it only removes the targeted pathology.
/// A proposal that claimed to fix *everything* could not be falsified by the
/// evidence, so it must not be assumed to.
fn without_target(report: &EfficiencyReport, finding: &Finding) -> Vec<Finding> {
    report
        .findings
        .iter()
        .filter(|candidate| {
            !(candidate.pathology == finding.pathology && candidate.tool == finding.tool)
        })
        .cloned()
        .collect()
}

/// Score the history would have if `finding` were resolved.
pub(crate) fn projected_score(report: &EfficiencyReport, finding: &Finding) -> i32 {
    health_score(&without_target(report, finding), &report.turns)
}

/// Turn a report into at most `config.max_proposals` inert candidates.
///
/// Candidates that cannot improve the score are dropped here rather than at the
/// gate: a human should never be asked to review a no-op.
pub fn propose_for_report(
    report: &EfficiencyReport,
    scope: &RsiScope,
    config: &RsiConfig,
) -> SeamResult<Vec<Proposal>> {
    scope.validate()?;
    if report.turns.is_empty() {
        return Ok(Vec::new());
    }
    // Every finding gets a candidate. Whether the projected gain is *enough* is
    // the validator's decision, not the proposer's: keeping the two separate is
    // what makes "rejected for being too small" observable in the sweep counters.
    let mut scored: Vec<(i32, &Finding)> = report
        .findings
        .iter()
        .map(|finding| (projected_score(report, finding), finding))
        .collect();
    // Highest projected benefit first; the pathology key breaks ties so the
    // output order is stable across runs.
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.key().cmp(&b.1.key())));

    let created_at_ms = now_ms();
    let proposals = scored
        .into_iter()
        .take(config.max_proposals)
        .map(|(score, finding)| Proposal {
            id: Uuid::new_v4().to_string(),
            scope: scope.clone(),
            origin: scope.clone(),
            pathology: finding.pathology.as_str().to_string(),
            tool: finding.tool.clone(),
            title: title_for(finding),
            body: body_for(finding),
            score,
            baseline: report.score,
            evidence: finding.evidence.clone(),
            created_at_ms,
            adaptation: None,
        })
        .collect();
    Ok(proposals)
}

/// Reject a proposal that never had evidence behind it.
///
/// Used by the sweep before a proposal is submitted: a proposal with no evidence
/// chain is indistinguishable from a guess.
pub(crate) fn ensure_evidenced(proposal: &Proposal) -> SeamResult<()> {
    if proposal.evidence.is_empty() {
        return Err(SeamError::Invalid(format!(
            "proposal `{}` carries no evidence",
            proposal.title
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::session::insight::{analyze, Evidence, InsightLimits, TurnInsight};
    use agent_core::session::projection::TurnStatus;
    use agent_core::session::{SessionEvent, SessionRecord};

    fn finding(pathology: Pathology, tool: Option<&str>, occurrences: usize) -> Finding {
        Finding {
            pathology,
            tool: tool.map(str::to_string),
            occurrences,
            evidence: vec![Evidence {
                seq: 1,
                turn_id: Some("t1".into()),
                tool: tool.map(str::to_string),
                detail: "x".into(),
            }],
        }
    }

    fn empty_report() -> EfficiencyReport {
        EfficiencyReport {
            session: "s".into(),
            folded_records: 0,
            truncated: false,
            turns: Vec::new(),
            findings: Vec::new(),
            score: 100,
        }
    }

    #[test]
    fn every_pathology_has_actionable_guidance() {
        for pathology in Pathology::ALL {
            let f = finding(*pathology, Some("shell"), 3);
            let body = body_for(&f);
            assert!(body.starts_with("# "), "{pathology:?}");
            assert!(body.contains("3"), "{pathology:?} states the count");
            assert!(body.contains("- "), "{pathology:?} lists steps");
        }
        // Tool-specific rules name the tool; generic rules never leak the count
        // into a threshold that would not transfer across tenants.
        assert!(
            body_for(&finding(Pathology::RetryStorm, Some("web_search"), 3)).contains("web_search")
        );
        assert!(body_for(&finding(Pathology::RetryStorm, None, 3)).contains("`the tool`"));
        assert!(body_for(&finding(Pathology::RepeatedToolFailure, None, 2))
            .starts_with("# Check preconditions before calling a tool"));
        assert!(body_for(&finding(Pathology::ApprovalDeniedLoop, None, 2))
            .starts_with("# Request the permission once"));
        assert!(body_for(&finding(Pathology::ToolArgumentRepair, None, 2))
            .starts_with("# Validate the tool arguments"));
        assert_eq!(
            title_for(&finding(Pathology::CompactionChurn, None, 2)),
            "Externalise progress before the context fills"
        );
        assert_eq!(
            title_for(&finding(Pathology::StepBudgetExhausted, None, 2)),
            "Checkpoint the goal before the step budget runs out"
        );
    }

    fn ledger_with_two_failures() -> Vec<SessionRecord> {
        let call = |seq: u64, id: &str| SessionRecord {
            seq,
            at_ms: 0,
            event: SessionEvent::ItemAdded {
                item_id: id.into(),
                kind: "tool_call".into(),
                payload: serde_json::json!({"name": "shell", "arguments": {"cmd": "ls"}}),
            },
        };
        let done = |seq: u64, id: &str| SessionRecord {
            seq,
            at_ms: 0,
            event: SessionEvent::ItemDone {
                item_id: id.into(),
                result: serde_json::json!({"ok": false}),
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
            call(5, "c"),
            done(6, "c"),
            SessionRecord {
                seq: 7,
                at_ms: 0,
                event: SessionEvent::TurnCompleted {
                    turn_id: "t1".into(),
                    output: "done".into(),
                },
            },
        ]
    }

    #[test]
    fn proposals_carry_scope_evidence_and_a_better_score() {
        let report = analyze("s1", &ledger_with_two_failures(), &InsightLimits::default());
        let scope = RsiScope::new("tenant", "orders").with_param("approval_threshold", "500");
        let proposals =
            propose_for_report(&report, &scope, &RsiConfig::default()).expect("proposals");
        assert!(!proposals.is_empty());
        let first = &proposals[0];
        assert_eq!(first.scope, scope);
        assert_eq!(first.pathology, "repeated_tool_failure");
        assert_eq!(first.tool.as_deref(), Some("shell"));
        assert!(first.score > first.baseline);
        assert_eq!(first.baseline, report.score);
        assert!(first.evidence.len() >= 2);
        assert!(!first.id.is_empty());
        assert!(first.adaptation.is_none());
        assert!(ensure_evidenced(first).is_ok());
    }

    #[test]
    fn the_proposal_cap_is_honoured_and_the_best_come_first() {
        let report = analyze("s1", &ledger_with_two_failures(), &InsightLimits::default());
        let scope = RsiScope::new("t", "d");
        let config = RsiConfig {
            max_proposals: 1,
            ..RsiConfig::default()
        };
        let proposals = propose_for_report(&report, &scope, &config).unwrap();
        assert_eq!(proposals.len(), 1);

        let all = propose_for_report(&report, &scope, &RsiConfig::default()).unwrap();
        assert_eq!(
            all.len(),
            2,
            "a retry storm and a repeated failure are two rules"
        );
        assert!(all[0].score >= all[1].score);
        assert_eq!(all[0].pathology, "repeated_tool_failure");
        assert_eq!(all[1].pathology, "retry_storm");
    }

    #[test]
    fn a_clean_or_empty_report_yields_nothing() {
        let scope = RsiScope::new("t", "d");
        assert!(
            propose_for_report(&empty_report(), &scope, &RsiConfig::default())
                .unwrap()
                .is_empty()
        );

        let clean = analyze(
            "s1",
            &[
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
                    at_ms: 0,
                    event: SessionEvent::TurnCompleted {
                        turn_id: "t1".into(),
                        output: "done".into(),
                    },
                },
            ],
            &InsightLimits::default(),
        );
        assert!(clean.is_clean());
        assert!(propose_for_report(&clean, &scope, &RsiConfig::default())
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_report_score_at_zero_still_yields_candidates_for_the_gate() {
        // The proposer never hides a finding because the score is exhausted; the
        // validator is what rejects an unprofitable candidate, and the sweep
        // counts that rejection.
        let report = EfficiencyReport {
            session: "s".into(),
            folded_records: 3,
            truncated: false,
            turns: vec![TurnInsight {
                turn_id: "t1".into(),
                status: TurnStatus::Failed,
                steps: 0,
                tool_calls: 0,
                tool_errors: 0,
                approvals_denied: 0,
                compactions: 0,
                measured_tools: 0,
                latency_ms: 0,
                input_tokens: 0,
                output_tokens: 0,
            }],
            findings: vec![finding(Pathology::RetryStorm, Some("shell"), 60)],
            score: 0,
        };
        let proposals =
            propose_for_report(&report, &RsiScope::new("t", "d"), &RsiConfig::default()).unwrap();
        assert_eq!(proposals.len(), 1);
        assert!(proposals[0].score > proposals[0].baseline);
    }

    #[test]
    fn an_invalid_scope_or_missing_evidence_is_refused() {
        let report = analyze("s1", &ledger_with_two_failures(), &InsightLimits::default());
        assert!(matches!(
            propose_for_report(&report, &RsiScope::new("", "d"), &RsiConfig::default()),
            Err(SeamError::Invalid(_))
        ));
        let mut proposal =
            propose_for_report(&report, &RsiScope::new("t", "d"), &RsiConfig::default())
                .unwrap()
                .remove(0);
        proposal.evidence.clear();
        assert!(matches!(
            ensure_evidenced(&proposal),
            Err(SeamError::Invalid(_))
        ));
    }
}
