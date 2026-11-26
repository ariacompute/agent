//! Efficiency insight: the deterministic fold RSI runs over a session ledger.
//!
//! Recursive self-improvement needs one thing before it can improve anything:
//! proof that something actually went wrong. This module is that proof machine —
//! it reads a session's append-only [`SessionRecord`] log (the event ledger) and
//! folds it into an [`EfficiencyReport`] whose [`Finding`]s each carry an
//! evidence chain of `seq` numbers, so a claim like "this tool failed twice" can
//! always be traced back to the exact events that justified it.
//!
//! Two properties are load-bearing:
//!
//! * **Pure.** No I/O, no clock, no model call, no randomness. The same records
//!   always produce the same report, which is what makes an improvement
//!   proposal reproducible and reviewable (ADR-0026).
//! * **Bounded.** A long session cannot turn a report into an unbounded
//!   allocation: records, turns and per-finding evidence are all capped, and
//!   truncation is reported rather than hidden.
//!
//! What this module deliberately does *not* do: decide what to change. Turning a
//! [`Finding`] into a proposal is the RSI engine's job, so the diagnosis stays
//! reusable and the proposal rule table stays replaceable.

use super::projection::{SessionProjection, TurnStatus};
use super::{SessionEvent, SessionRecord};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A recurring inefficiency the ledger can *prove* happened.
///
/// Every variant is derived from events that exist today; adding one is a
/// diagnosis improvement, not an event-contract change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Pathology {
    /// The same tool failed at least `repeated_failure` times in the session.
    RepeatedToolFailure,
    /// One turn called the same tool at least `retry_storm` times.
    RetryStorm,
    /// The same tool was denied at least `denied_loop` times.
    ApprovalDeniedLoop,
    /// One turn compacted context at least `compaction_churn` times.
    CompactionChurn,
    /// A turn hit the step budget, or never reached a terminal event.
    StepBudgetExhausted,
    /// One turn called the same tool with different arguments and also failed.
    ToolArgumentRepair,
}

impl Pathology {
    /// Stable wire/`key` spelling (matches the serde name).
    pub fn as_str(self) -> &'static str {
        match self {
            Pathology::RepeatedToolFailure => "repeated_tool_failure",
            Pathology::RetryStorm => "retry_storm",
            Pathology::ApprovalDeniedLoop => "approval_denied_loop",
            Pathology::CompactionChurn => "compaction_churn",
            Pathology::StepBudgetExhausted => "step_budget_exhausted",
            Pathology::ToolArgumentRepair => "tool_argument_repair",
        }
    }

    /// Every variant, in declaration order (used by stable ordering and docs).
    pub const ALL: &'static [Pathology] = &[
        Pathology::RepeatedToolFailure,
        Pathology::RetryStorm,
        Pathology::ApprovalDeniedLoop,
        Pathology::CompactionChurn,
        Pathology::StepBudgetExhausted,
        Pathology::ToolArgumentRepair,
    ];
}

/// One event that supports a [`Finding`].
///
/// `detail` is a fixed short label, never record content: the report must not
/// carry prompts, arguments or tool output into logs (AGENTS.md rule 5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Evidence {
    /// Ledger position of the supporting event.
    pub seq: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// Short fixed label such as `tool_error` or `approval_deny`.
    pub detail: String,
}

impl Evidence {
    fn new(seq: u64, turn_id: Option<&str>, tool: Option<&str>, detail: &str) -> Self {
        Self {
            seq,
            turn_id: turn_id.map(str::to_string),
            tool: tool.map(str::to_string),
            detail: detail.to_string(),
        }
    }
}

/// A diagnosed inefficiency plus the evidence chain behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub pathology: Pathology,
    /// Tool the finding is scoped to, when it is tool-specific.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// How many times the pattern occurred (may exceed `evidence.len()`).
    pub occurrences: usize,
    /// Supporting events, capped at [`InsightLimits::max_evidence`].
    pub evidence: Vec<Evidence>,
}

impl Finding {
    /// Stable key the proposal layer keys playbooks by.
    pub fn key(&self) -> String {
        match &self.tool {
            Some(tool) => format!("{}:{}", self.pathology.as_str(), tool),
            None => self.pathology.as_str().to_string(),
        }
    }
}

/// Per-turn counters folded from the ledger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TurnInsight {
    pub turn_id: String,
    pub status: TurnStatus,
    pub steps: usize,
    pub tool_calls: usize,
    pub tool_errors: usize,
    pub approvals_denied: usize,
    pub compactions: usize,
    /// Calls that carried a measured latency.
    pub measured_tools: usize,
    /// Sum of measured tool latencies.
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
}

/// The diagnosis produced for one session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EfficiencyReport {
    pub session: String,
    /// Records actually folded (after the `max_records` window).
    pub folded_records: usize,
    /// True when the window or the turn cap dropped older history.
    pub truncated: bool,
    pub turns: Vec<TurnInsight>,
    pub findings: Vec<Finding>,
    /// Health score in `0..=100`; 100 means "nothing diagnosable".
    pub score: i32,
}

impl EfficiencyReport {
    /// Count of turns that ended in `failed`.
    pub fn failed_turns(&self) -> usize {
        self.turns
            .iter()
            .filter(|t| t.status == TurnStatus::Failed)
            .count()
    }

    /// Findings of one kind, in stable order.
    pub fn findings_of(&self, pathology: Pathology) -> Vec<&Finding> {
        self.findings
            .iter()
            .filter(|f| f.pathology == pathology)
            .collect()
    }

    /// True when there is nothing to improve.
    pub fn is_clean(&self) -> bool {
        self.findings.is_empty() && self.failed_turns() == 0
    }
}

/// Thresholds and hard caps for one analysis.
///
/// Defaults are the production values; the RSI engine passes its configured
/// [`crate::seams::RsiConfig`] through [`InsightLimits::from_config`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InsightLimits {
    pub max_records: usize,
    pub max_turns: usize,
    pub max_evidence: usize,
    pub repeated_failure: usize,
    pub retry_storm: usize,
    pub denied_loop: usize,
    pub compaction_churn: usize,
}

impl Default for InsightLimits {
    fn default() -> Self {
        Self {
            max_records: 2_000,
            max_turns: 50,
            max_evidence: 8,
            repeated_failure: 2,
            retry_storm: 3,
            denied_loop: 2,
            compaction_churn: 2,
        }
    }
}

impl InsightLimits {
    /// Clamp every value into a usable range.
    ///
    /// A zero cap would either disable the report silently or divide the window
    /// into nothing, so zero is *not* a supported value: it becomes the minimum
    /// (1) instead of failing an already-completed turn.
    pub fn sanitized(&self) -> Self {
        Self {
            max_records: self.max_records.max(1),
            max_turns: self.max_turns.max(1),
            max_evidence: self.max_evidence.max(1),
            repeated_failure: self.repeated_failure.max(1),
            retry_storm: self.retry_storm.max(1),
            denied_loop: self.denied_loop.max(1),
            compaction_churn: self.compaction_churn.max(1),
        }
    }

    /// Build limits from the deployment configuration (single source of truth).
    pub fn from_config(config: &crate::seams::RsiConfig) -> Self {
        Self {
            max_records: config.max_records,
            max_turns: config.max_turns,
            max_evidence: config.max_evidence,
            repeated_failure: config.repeated_failure,
            retry_storm: config.retry_storm,
            denied_loop: config.denied_loop,
            compaction_churn: config.compaction_churn,
        }
    }
}

/// One tool call, as folded from `item_added` + `item_done`.
#[derive(Debug, Clone)]
struct CallFold {
    tool: String,
    arguments: String,
    added_seq: u64,
    done_seq: u64,
    is_error: bool,
    latency_ms: Option<u64>,
}

impl CallFold {
    /// Sequence number that best identifies the call: the completion when it
    /// happened, otherwise the request.
    fn anchor_seq(&self) -> u64 {
        if self.done_seq > 0 {
            self.done_seq
        } else {
            self.added_seq
        }
    }
}

#[derive(Debug, Default)]
struct TurnFold {
    turn_id: String,
    steps: usize,
    calls: Vec<CallFold>,
    denials: Vec<(u64, String)>,
    compaction_seqs: Vec<u64>,
    input_tokens: u64,
    output_tokens: u64,
    loop_guard: Option<u64>,
    last_seq: u64,
}

/// Fold one session ledger into a report.
///
/// The window keeps the **most recent** `max_records` records: an improvement
/// proposal is about what the agent is doing lately, not about what it did a
/// thousand turns ago.
pub fn analyze(
    session: &str,
    records: &[SessionRecord],
    limits: &InsightLimits,
) -> EfficiencyReport {
    let limits = limits.sanitized();
    let keep_from = records.len().saturating_sub(limits.max_records);
    let window = &records[keep_from..];
    let projection = SessionProjection::replay(window);
    let turns = fold_turns(window);

    let mut findings = Vec::new();
    findings.extend(repeated_tool_failures(&turns, &limits));
    findings.extend(retry_storms(&turns, &limits));
    findings.extend(denied_loops(&turns, &limits));
    findings.extend(compaction_churn(&turns, &limits));
    findings.extend(step_budget(&turns, &projection));
    findings.extend(argument_repairs(&turns, &limits));
    sort_findings(&mut findings);

    let mut insights: Vec<TurnInsight> = turns
        .iter()
        .enumerate()
        .map(|(idx, turn)| {
            // Both folds push exactly one entry per `TurnStarted`, so the
            // projection is the authoritative source for the turn status.
            let status = projection.turns[idx].status;
            let measured: Vec<&CallFold> = turn
                .calls
                .iter()
                .filter(|c| c.latency_ms.is_some())
                .collect();
            TurnInsight {
                turn_id: turn.turn_id.clone(),
                status,
                steps: turn.steps,
                tool_calls: turn.calls.len(),
                tool_errors: turn.calls.iter().filter(|c| c.is_error).count(),
                approvals_denied: turn.denials.len(),
                compactions: turn.compaction_seqs.len(),
                measured_tools: measured.len(),
                latency_ms: measured.iter().filter_map(|c| c.latency_ms).sum(),
                input_tokens: turn.input_tokens,
                output_tokens: turn.output_tokens,
            }
        })
        .collect();

    let truncated = keep_from > 0 || insights.len() > limits.max_turns;
    if insights.len() > limits.max_turns {
        insights.drain(..insights.len() - limits.max_turns);
    }

    let score = health_score(&findings, insights.as_slice());
    EfficiencyReport {
        session: session.to_string(),
        folded_records: window.len(),
        truncated,
        turns: insights,
        findings,
        score,
    }
}

fn fold_turns(records: &[SessionRecord]) -> Vec<TurnFold> {
    let mut turns: Vec<TurnFold> = Vec::new();
    let mut open: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for record in records {
        match &record.event {
            SessionEvent::TurnStarted { turn_id, .. } => turns.push(TurnFold {
                turn_id: turn_id.clone(),
                ..TurnFold::default()
            }),
            SessionEvent::Step { phase, .. } => {
                if let Some(turn) = turns.last_mut() {
                    turn.steps += 1;
                    turn.last_seq = record.seq;
                    if phase == "loop_guard" {
                        turn.loop_guard = Some(record.seq);
                    }
                }
            }
            SessionEvent::ItemAdded {
                item_id,
                kind,
                payload,
            } => {
                if kind != "tool_call" {
                    continue;
                }
                let next_index = turns.len();
                if let Some(turn) = turns.last_mut() {
                    open.insert(item_id.clone(), (next_index - 1, turn.calls.len()));
                    turn.calls.push(CallFold {
                        tool: payload
                            .get("name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown")
                            .to_string(),
                        arguments: payload
                            .get("arguments")
                            .map(|v| v.to_string())
                            .unwrap_or_default(),
                        added_seq: record.seq,
                        done_seq: 0,
                        is_error: false,
                        latency_ms: None,
                    });
                    turn.last_seq = record.seq;
                }
            }
            SessionEvent::ItemDone {
                item_id,
                is_error,
                latency_ms,
                ..
            } => {
                if let Some((turn_idx, call_idx)) = open.remove(item_id) {
                    let call = &mut turns[turn_idx].calls[call_idx];
                    call.done_seq = record.seq;
                    call.is_error = *is_error;
                    call.latency_ms = *latency_ms;
                    turns[turn_idx].last_seq = record.seq;
                }
            }
            SessionEvent::Approval { tool, decision, .. } => {
                if let Some(turn) = turns.last_mut() {
                    turn.last_seq = record.seq;
                    if decision == "deny" {
                        turn.denials.push((record.seq, tool.clone()));
                    }
                }
            }
            SessionEvent::Usage {
                input_tokens,
                output_tokens,
                ..
            } => {
                if let Some(turn) = turns.last_mut() {
                    turn.input_tokens += input_tokens;
                    turn.output_tokens += output_tokens;
                }
            }
            SessionEvent::Compacted { .. } => {
                if let Some(turn) = turns.last_mut() {
                    turn.compaction_seqs.push(record.seq);
                    turn.last_seq = record.seq;
                }
            }
            SessionEvent::TurnCompleted { .. } | SessionEvent::TurnFailed { .. } => {
                if let Some(turn) = turns.last_mut() {
                    turn.last_seq = record.seq;
                }
            }
            SessionEvent::TextDelta { .. }
            | SessionEvent::TextDone { .. }
            | SessionEvent::TitleSet { .. } => {}
        }
    }
    turns
}

fn repeated_tool_failures(turns: &[TurnFold], limits: &InsightLimits) -> Vec<Finding> {
    let mut per_tool: BTreeMap<&str, Vec<(u64, &str)>> = BTreeMap::new();
    for turn in turns {
        for call in &turn.calls {
            if call.is_error {
                per_tool
                    .entry(call.tool.as_str())
                    .or_default()
                    .push((call.anchor_seq(), turn.turn_id.as_str()));
            }
        }
    }
    per_tool
        .into_iter()
        .filter_map(|(tool, hits)| {
            (hits.len() >= limits.repeated_failure).then(|| Finding {
                pathology: Pathology::RepeatedToolFailure,
                tool: Some(tool.to_string()),
                occurrences: hits.len(),
                evidence: hits
                    .iter()
                    .take(limits.max_evidence)
                    .map(|(seq, turn)| Evidence::new(*seq, Some(turn), Some(tool), "tool_error"))
                    .collect(),
            })
        })
        .collect()
}

fn retry_storms(turns: &[TurnFold], limits: &InsightLimits) -> Vec<Finding> {
    let mut per_tool: BTreeMap<&str, Vec<(u64, &str)>> = BTreeMap::new();
    for turn in turns {
        let mut per_call_count: BTreeMap<&str, usize> = BTreeMap::new();
        for call in &turn.calls {
            *per_call_count.entry(call.tool.as_str()).or_default() += 1;
        }
        for (tool, count) in per_call_count {
            if count < limits.retry_storm {
                continue;
            }
            let seqs = per_tool.entry(tool).or_default();
            seqs.extend(
                turn.calls
                    .iter()
                    .filter(|c| c.tool == tool)
                    .map(|c| (c.anchor_seq(), turn.turn_id.as_str())),
            );
        }
    }
    per_tool
        .into_iter()
        .map(|(tool, hits)| Finding {
            pathology: Pathology::RetryStorm,
            tool: Some(tool.to_string()),
            occurrences: hits.len(),
            evidence: hits
                .iter()
                .take(limits.max_evidence)
                .map(|(seq, turn)| Evidence::new(*seq, Some(turn), Some(tool), "retry"))
                .collect(),
        })
        .collect()
}

fn denied_loops(turns: &[TurnFold], limits: &InsightLimits) -> Vec<Finding> {
    let mut per_tool: BTreeMap<&str, Vec<(u64, &str)>> = BTreeMap::new();
    for turn in turns {
        for (seq, tool) in &turn.denials {
            per_tool
                .entry(tool.as_str())
                .or_default()
                .push((*seq, turn.turn_id.as_str()));
        }
    }
    per_tool
        .into_iter()
        .filter_map(|(tool, hits)| {
            (hits.len() >= limits.denied_loop).then(|| Finding {
                pathology: Pathology::ApprovalDeniedLoop,
                tool: Some(tool.to_string()),
                occurrences: hits.len(),
                evidence: hits
                    .iter()
                    .take(limits.max_evidence)
                    .map(|(seq, turn)| Evidence::new(*seq, Some(turn), Some(tool), "approval_deny"))
                    .collect(),
            })
        })
        .collect()
}

fn compaction_churn(turns: &[TurnFold], limits: &InsightLimits) -> Vec<Finding> {
    let mut evidence = Vec::new();
    let mut occurrences = 0;
    for turn in turns {
        if turn.compaction_seqs.len() < limits.compaction_churn {
            continue;
        }
        occurrences += turn.compaction_seqs.len();
        evidence.extend(
            turn.compaction_seqs
                .iter()
                .map(|seq| Evidence::new(*seq, Some(&turn.turn_id), None, "compaction")),
        );
    }
    if occurrences == 0 {
        return Vec::new();
    }
    evidence.truncate(limits.max_evidence);
    vec![Finding {
        pathology: Pathology::CompactionChurn,
        tool: None,
        occurrences,
        evidence,
    }]
}

fn step_budget(turns: &[TurnFold], projection: &SessionProjection) -> Vec<Finding> {
    let mut evidence = Vec::new();
    let mut occurrences = 0;
    for (idx, turn) in turns.iter().enumerate() {
        let status = projection.turns[idx].status;
        match turn.loop_guard {
            Some(seq) => {
                occurrences += 1;
                evidence.push(Evidence::new(seq, Some(&turn.turn_id), None, "loop_guard"));
            }
            None if status == TurnStatus::Running => {
                occurrences += 1;
                evidence.push(Evidence::new(
                    turn.last_seq,
                    Some(&turn.turn_id),
                    None,
                    "unclosed_turn",
                ));
            }
            None => {}
        }
    }
    if occurrences == 0 {
        return Vec::new();
    }
    vec![Finding {
        pathology: Pathology::StepBudgetExhausted,
        tool: None,
        occurrences,
        evidence,
    }]
}

fn argument_repairs(turns: &[TurnFold], limits: &InsightLimits) -> Vec<Finding> {
    let mut per_tool: BTreeMap<&str, Vec<(u64, &str)>> = BTreeMap::new();
    for turn in turns {
        let mut by_tool: BTreeMap<&str, Vec<&CallFold>> = BTreeMap::new();
        for call in &turn.calls {
            by_tool.entry(call.tool.as_str()).or_default().push(call);
        }
        for (tool, calls) in by_tool {
            let distinct: BTreeMap<&str, ()> =
                calls.iter().map(|c| (c.arguments.as_str(), ())).collect();
            let errored = calls.iter().any(|c| c.is_error);
            if distinct.len() < 2 || !errored {
                continue;
            }
            let hits = per_tool.entry(tool).or_default();
            hits.extend(
                calls
                    .iter()
                    .map(|c| (c.anchor_seq(), turn.turn_id.as_str())),
            );
        }
    }
    per_tool
        .into_iter()
        .map(|(tool, hits)| Finding {
            pathology: Pathology::ToolArgumentRepair,
            tool: Some(tool.to_string()),
            occurrences: hits.len(),
            evidence: hits
                .iter()
                .take(limits.max_evidence)
                .map(|(seq, turn)| Evidence::new(*seq, Some(turn), Some(tool), "arguments_changed"))
                .collect(),
        })
        .collect()
}

fn sort_findings(findings: &mut [Finding]) {
    findings.sort_by(|a, b| {
        a.pathology
            .cmp(&b.pathology)
            .then_with(|| a.tool.cmp(&b.tool))
    });
}

/// Score is a coarse health signal, not a quality metric: it exists so a report
/// can be compared against a baseline in one number, and it must be monotonic —
/// more findings and more failed turns can only lower it.
///
/// Public because the RSI validator replays a report with a proposal applied and
/// must score it with **exactly** this function; two scoring rules would make a
/// "validated" improvement meaningless.
pub fn health_score(findings: &[Finding], turns: &[TurnInsight]) -> i32 {
    let occurrences: usize = findings.iter().map(|f| f.occurrences).sum();
    let failed = turns
        .iter()
        .filter(|t| t.status == TurnStatus::Failed)
        .count();
    let penalty = 10 * findings.len() as i32 + 2 * occurrences as i32 + 15 * failed as i32;
    (100 - penalty).clamp(0, 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(seq: u64, event: SessionEvent) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: 0,
            event,
        }
    }

    fn started(seq: u64, turn: &str) -> SessionRecord {
        rec(
            seq,
            SessionEvent::TurnStarted {
                turn_id: turn.into(),
                input: "do the thing".into(),
            },
        )
    }

    fn done(seq: u64, turn: &str) -> SessionRecord {
        rec(
            seq,
            SessionEvent::TurnCompleted {
                turn_id: turn.into(),
                output: "ok".into(),
            },
        )
    }

    fn failed(seq: u64, turn: &str) -> SessionRecord {
        rec(
            seq,
            SessionEvent::TurnFailed {
                turn_id: turn.into(),
                message: "boom".into(),
            },
        )
    }

    fn call(seq: u64, id: &str, tool: &str, args: &str) -> SessionRecord {
        rec(
            seq,
            SessionEvent::ItemAdded {
                item_id: id.into(),
                kind: "tool_call".into(),
                payload: serde_json::json!({"name": tool, "arguments": args}),
            },
        )
    }

    fn call_done(seq: u64, id: &str, is_error: bool, latency: Option<u64>) -> SessionRecord {
        rec(
            seq,
            SessionEvent::ItemDone {
                item_id: id.into(),
                result: serde_json::json!({"ok": !is_error}),
                is_error,
                latency_ms: latency,
            },
        )
    }

    fn denial(seq: u64, tool: &str) -> SessionRecord {
        rec(
            seq,
            SessionEvent::Approval {
                id: format!("a{seq}"),
                tool: tool.into(),
                decision: "deny".into(),
                reason: None,
            },
        )
    }

    fn clean_session() -> Vec<SessionRecord> {
        vec![
            started(0, "t1"),
            rec(
                1,
                SessionEvent::Step {
                    phase: "model".into(),
                    label: None,
                },
            ),
            call(2, "i1", "fs_read", "{}"),
            call_done(3, "i1", false, Some(12)),
            rec(
                4,
                SessionEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 4,
                    model: "m".into(),
                },
            ),
            done(5, "t1"),
        ]
    }

    #[test]
    fn a_clean_session_has_no_findings_and_a_perfect_score() {
        let report = analyze("s1", &clean_session(), &InsightLimits::default());
        assert!(report.is_clean());
        assert_eq!(report.score, 100);
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.folded_records, 6);
        assert!(!report.truncated);
        assert_eq!(report.turns[0].steps, 1);
        assert_eq!(report.turns[0].tool_calls, 1);
        assert_eq!(report.turns[0].tool_errors, 0);
        assert_eq!(report.turns[0].measured_tools, 1);
        assert_eq!(report.turns[0].latency_ms, 12);
        assert_eq!(report.turns[0].input_tokens, 10);
        assert_eq!(report.turns[0].output_tokens, 4);
        assert_eq!(report.turns[0].status, TurnStatus::Completed);
        assert_eq!(report.failed_turns(), 0);
        assert!(report.findings_of(Pathology::RetryStorm).is_empty());
    }

    #[test]
    fn an_empty_ledger_is_reported_as_clean() {
        let report = analyze("s1", &[], &InsightLimits::default());
        assert!(report.turns.is_empty());
        assert!(report.findings.is_empty());
        assert_eq!(report.score, 100);
        assert_eq!(report.folded_records, 0);
    }

    #[test]
    fn repeated_tool_failures_carry_an_evidence_chain() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "shell", "{}"),
            call_done(2, "a", true, Some(60_000)),
            call(3, "b", "shell", "{}"),
            call_done(4, "b", true, Some(60_000)),
            done(5, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let found = report.findings_of(Pathology::RepeatedToolFailure);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tool.as_deref(), Some("shell"));
        assert_eq!(found[0].occurrences, 2);
        assert_eq!(found[0].evidence.len(), 2);
        assert_eq!(found[0].evidence[0].seq, 2);
        assert_eq!(found[0].evidence[0].turn_id.as_deref(), Some("t1"));
        assert_eq!(found[0].evidence[0].detail, "tool_error");
        assert_eq!(found[0].key(), "repeated_tool_failure:shell");
        assert!(report.score < 100);
        assert_eq!(report.turns[0].tool_errors, 2);
    }

    #[test]
    fn a_single_failure_is_below_the_threshold() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "shell", "{}"),
            call_done(2, "a", true, None),
            call(3, "b", "fs_read", "{}"),
            call_done(4, "b", false, None),
            done(5, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        assert!(report
            .findings_of(Pathology::RepeatedToolFailure)
            .is_empty());
        assert_eq!(report.turns[0].measured_tools, 0);
        assert_eq!(report.turns[0].latency_ms, 0);
    }

    #[test]
    fn a_retry_storm_inside_one_turn_is_detected_once_per_tool() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "web_search", "{\"q\":\"x\"}"),
            call_done(2, "a", false, None),
            call(3, "b", "web_search", "{\"q\":\"x\"}"),
            call_done(4, "b", false, None),
            call(5, "c", "web_search", "{\"q\":\"x\"}"),
            call_done(6, "c", false, None),
            done(7, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let storm = report.findings_of(Pathology::RetryStorm);
        assert_eq!(storm.len(), 1);
        assert_eq!(storm[0].occurrences, 3);
        assert_eq!(storm[0].evidence.len(), 3);
        assert_eq!(storm[0].evidence[2].detail, "retry");
    }

    #[test]
    fn an_unfinished_call_still_anchors_on_its_request() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "web_search", "{}"),
            call_done(2, "a", false, None),
            call(3, "b", "web_search", "{}"),
            call_done(4, "b", false, None),
            // Never completed: a cancelled or still-running third attempt.
            call(5, "c", "web_search", "{}"),
            done(6, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let storm = report.findings_of(Pathology::RetryStorm);
        assert_eq!(storm.len(), 1);
        assert_eq!(storm[0].evidence[2].seq, 5);
    }

    #[test]
    fn findings_of_one_pathology_are_ordered_by_tool() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "zeta", "{}"),
            call_done(2, "a", true, None),
            call(3, "b", "zeta", "{}"),
            call_done(4, "b", true, None),
            call(5, "c", "alpha", "{}"),
            call_done(6, "c", true, None),
            call(7, "d", "alpha", "{}"),
            call_done(8, "d", true, None),
            done(9, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let found = report.findings_of(Pathology::RepeatedToolFailure);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].tool.as_deref(), Some("alpha"));
        assert_eq!(found[1].tool.as_deref(), Some("zeta"));
    }

    #[test]
    fn denied_approvals_loop_on_the_same_tool() {
        let log = vec![
            started(0, "t1"),
            denial(1, "shell"),
            call(2, "a", "shell", "{}"),
            call_done(3, "a", true, None),
            denial(4, "shell"),
            done(5, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let denied = report.findings_of(Pathology::ApprovalDeniedLoop);
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].occurrences, 2);
        assert_eq!(report.turns[0].approvals_denied, 2);
    }

    #[test]
    fn compaction_churn_is_scoped_to_a_turn() {
        let log = vec![
            started(0, "t1"),
            rec(
                1,
                SessionEvent::Compacted {
                    fragment_id: "f1".into(),
                    removed: 3,
                    reason: "pressure".into(),
                },
            ),
            rec(
                2,
                SessionEvent::Compacted {
                    fragment_id: "f2".into(),
                    removed: 2,
                    reason: "pressure".into(),
                },
            ),
            done(3, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let churn = report.findings_of(Pathology::CompactionChurn);
        assert_eq!(churn.len(), 1);
        assert_eq!(churn[0].occurrences, 2);
        assert_eq!(churn[0].tool, None);
        assert_eq!(churn[0].key(), "compaction_churn");
        assert_eq!(report.turns[0].compactions, 2);
    }

    #[test]
    fn one_compaction_is_not_churn() {
        let log = vec![
            started(0, "t1"),
            rec(
                1,
                SessionEvent::Compacted {
                    fragment_id: "f1".into(),
                    removed: 1,
                    reason: "pressure".into(),
                },
            ),
            done(2, "t1"),
        ];
        assert!(analyze("s1", &log, &InsightLimits::default())
            .findings_of(Pathology::CompactionChurn)
            .is_empty());
    }

    #[test]
    fn loop_guard_and_unclosed_turns_both_count_as_budget_exhaustion() {
        let log = vec![
            started(0, "t1"),
            rec(
                1,
                SessionEvent::Step {
                    phase: "loop_guard".into(),
                    label: Some("steps".into()),
                },
            ),
            done(2, "t1"),
            started(3, "t2"),
            rec(
                4,
                SessionEvent::Step {
                    phase: "model".into(),
                    label: None,
                },
            ),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let budget = report.findings_of(Pathology::StepBudgetExhausted);
        assert_eq!(budget.len(), 1);
        assert_eq!(budget[0].occurrences, 2);
        assert_eq!(budget[0].evidence[0].detail, "loop_guard");
        assert_eq!(budget[0].evidence[1].detail, "unclosed_turn");
        assert_eq!(budget[0].evidence[1].seq, 4);
        assert_eq!(report.turns[1].status, TurnStatus::Running);
    }

    #[test]
    fn changed_arguments_after_a_failure_flag_a_repair_loop() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "fs_read", "{\"path\":\"a\"}"),
            call_done(2, "a", true, None),
            call(3, "b", "fs_read", "{\"path\":\"b\"}"),
            call_done(4, "b", false, None),
            done(5, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let repairs = report.findings_of(Pathology::ToolArgumentRepair);
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].occurrences, 2);
        assert_eq!(repairs[0].evidence[0].detail, "arguments_changed");
    }

    #[test]
    fn identical_arguments_are_not_a_repair_loop() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "fs_read", "{\"path\":\"a\"}"),
            call_done(2, "a", true, None),
            call(3, "b", "fs_read", "{\"path\":\"a\"}"),
            call_done(4, "b", true, None),
            done(5, "t1"),
        ];
        assert!(analyze("s1", &log, &InsightLimits::default())
            .findings_of(Pathology::ToolArgumentRepair)
            .is_empty());
    }

    #[test]
    fn findings_are_sorted_and_evidence_is_capped() {
        let mut log = vec![started(0, "t1")];
        let mut seq = 1;
        for i in 0..10 {
            log.push(call(seq, &format!("c{i}"), "shell", "{}"));
            log.push(call_done(seq + 1, &format!("c{i}"), true, None));
            seq += 2;
        }
        log.push(denial(seq, "shell"));
        log.push(denial(seq + 1, "shell"));
        log.push(done(seq + 2, "t1"));
        let limits = InsightLimits {
            max_evidence: 3,
            ..InsightLimits::default()
        };
        let report = analyze("s1", &log, &limits);
        assert_eq!(report.findings[0].pathology, Pathology::RepeatedToolFailure);
        assert_eq!(report.findings[0].occurrences, 10);
        assert_eq!(report.findings[0].evidence.len(), 3);
        assert_eq!(report.findings[1].pathology, Pathology::RetryStorm);
        assert_eq!(report.findings[2].pathology, Pathology::ApprovalDeniedLoop);
        assert_eq!(report.findings.len(), 3);
    }

    #[test]
    fn a_long_ledger_is_windowed_and_marked_truncated() {
        let mut log = vec![started(0, "old")];
        log.push(done(1, "old"));
        log.push(started(2, "new"));
        log.push(call(3, "a", "shell", "{}"));
        log.push(call_done(4, "a", false, None));
        log.push(done(5, "new"));
        let limits = InsightLimits {
            max_records: 4,
            ..InsightLimits::default()
        };
        let report = analyze("s1", &log, &limits);
        assert!(report.truncated);
        assert_eq!(report.folded_records, 4);
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.turns[0].turn_id, "new");
    }

    #[test]
    fn the_turn_cap_keeps_the_most_recent_turns() {
        let mut log = Vec::new();
        for i in 0..5u64 {
            let turn = format!("t{i}");
            log.push(started(i * 2, &turn));
            log.push(done(i * 2 + 1, &turn));
        }
        let limits = InsightLimits {
            max_turns: 2,
            ..InsightLimits::default()
        };
        let report = analyze("s1", &log, &limits);
        assert!(report.truncated);
        assert_eq!(report.turns.len(), 2);
        assert_eq!(report.turns[1].turn_id, "t4");
    }

    #[test]
    fn failed_turns_lower_the_score_even_without_findings() {
        let log = vec![started(0, "t1"), failed(1, "t1")];
        let report = analyze("s1", &log, &InsightLimits::default());
        assert!(report.findings.is_empty());
        assert_eq!(report.failed_turns(), 1);
        assert!(!report.is_clean());
        assert_eq!(report.score, 85);
        assert_eq!(report.turns[0].status, TurnStatus::Failed);
    }

    #[test]
    fn the_score_never_goes_negative() {
        let mut log = vec![started(0, "t1")];
        let mut seq = 1;
        for i in 0..20 {
            log.push(call(
                seq,
                &format!("c{i}"),
                "shell",
                &format!("{{\"i\":{i}}}"),
            ));
            log.push(call_done(seq + 1, &format!("c{i}"), true, None));
            seq += 2;
        }
        for _ in 0..3 {
            log.push(denial(seq, "shell"));
            seq += 1;
        }
        log.push(failed(seq, "t1"));
        assert_eq!(analyze("s1", &log, &InsightLimits::default()).score, 0);
    }

    #[test]
    fn limits_are_clamped_and_derive_from_config() {
        let raw = InsightLimits {
            max_records: 0,
            max_turns: 0,
            max_evidence: 0,
            repeated_failure: 0,
            retry_storm: 0,
            denied_loop: 0,
            compaction_churn: 0,
        };
        let clamped = raw.sanitized();
        assert_eq!(clamped.max_records, 1);
        assert_eq!(clamped.max_turns, 1);
        assert_eq!(clamped.max_evidence, 1);
        assert_eq!(clamped.repeated_failure, 1);
        assert_eq!(clamped.retry_storm, 1);
        assert_eq!(clamped.denied_loop, 1);
        assert_eq!(clamped.compaction_churn, 1);

        let config = crate::seams::RsiConfig::default();
        let derived = InsightLimits::from_config(&config);
        assert_eq!(derived.max_records, config.max_records);
        assert_eq!(derived.max_turns, config.max_turns);
        assert_eq!(derived.max_evidence, config.max_evidence);
        assert_eq!(derived.repeated_failure, config.repeated_failure);
        assert_eq!(derived.retry_storm, config.retry_storm);
        assert_eq!(derived.denied_loop, config.denied_loop);
        assert_eq!(derived.compaction_churn, config.compaction_churn);
    }

    #[test]
    fn pathology_labels_and_order_are_stable() {
        assert_eq!(
            Pathology::ALL
                .iter()
                .map(|p| p.as_str())
                .collect::<Vec<_>>(),
            vec![
                "repeated_tool_failure",
                "retry_storm",
                "approval_denied_loop",
                "compaction_churn",
                "step_budget_exhausted",
                "tool_argument_repair",
            ]
        );
        let json = serde_json::to_value(Pathology::ToolArgumentRepair).unwrap();
        assert_eq!(json, "tool_argument_repair");
        let back: Pathology = serde_json::from_value(json).unwrap();
        assert_eq!(back, Pathology::ToolArgumentRepair);
    }

    #[test]
    fn reports_roundtrip_through_json() {
        let log = vec![
            started(0, "t1"),
            call(1, "a", "shell", "{}"),
            call_done(2, "a", true, Some(5)),
            denial(3, "shell"),
            done(4, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let json = serde_json::to_string(&report).unwrap();
        let back: EfficiencyReport = serde_json::from_str(&json).unwrap();
        assert_eq!(back, report);
        assert_eq!(
            serde_json::to_value(&report.turns[0]).unwrap()["status"],
            "completed"
        );
    }

    #[test]
    fn non_tool_items_and_orphan_completions_are_ignored() {
        let log = vec![
            rec(
                0,
                SessionEvent::ItemAdded {
                    item_id: "todo".into(),
                    kind: "todo".into(),
                    payload: serde_json::json!({"todos": []}),
                },
            ),
            rec(
                1,
                SessionEvent::ItemDone {
                    item_id: "missing".into(),
                    result: serde_json::json!(null),
                    is_error: true,
                    latency_ms: None,
                },
            ),
            started(2, "t1"),
            rec(3, SessionEvent::TextDelta { text: "a".into() }),
            rec(4, SessionEvent::TextDone { text: "a".into() }),
            rec(
                5,
                SessionEvent::TitleSet {
                    title: "T".into(),
                    source: "first_prompt".into(),
                },
            ),
            done(6, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        assert_eq!(report.turns.len(), 1);
        assert_eq!(report.turns[0].tool_calls, 0);
        assert!(report.findings.is_empty());
    }

    #[test]
    fn tool_calls_without_a_name_stay_attributable() {
        let log = vec![
            started(0, "t1"),
            rec(
                1,
                SessionEvent::ItemAdded {
                    item_id: "a".into(),
                    kind: "tool_call".into(),
                    payload: serde_json::json!({}),
                },
            ),
            call_done(2, "a", true, None),
            rec(
                3,
                SessionEvent::ItemAdded {
                    item_id: "b".into(),
                    kind: "tool_call".into(),
                    payload: serde_json::json!({}),
                },
            ),
            call_done(4, "b", true, None),
            done(5, "t1"),
        ];
        let report = analyze("s1", &log, &InsightLimits::default());
        let found = report.findings_of(Pathology::RepeatedToolFailure);
        assert_eq!(found[0].tool.as_deref(), Some("unknown"));
        assert_eq!(found[0].key(), "repeated_tool_failure:unknown");
    }
}
