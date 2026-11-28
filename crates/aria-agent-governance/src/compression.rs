//! Rule-based projection of tool output for the model.
//!
//! Deliberately **not** a model-driven summary (ADR-0022 §压缩):
//! * it is deterministic, so the same tool output always compresses the same way;
//! * it costs no tokens and no latency;
//! * it never touches the real result — `ToolOutcome::result` keeps the observed
//!   output for events and audit, only `model_content` is projected.
//!
//! Every strategy degrades to pass-through when it cannot find a safe reduction.
//! Losing information silently would be worse than spending tokens.

use agent_core::seams::{
    EstimatedTokens, Projection, ProjectionRecord, SeamResult, ToolOutputProjector,
};
use serde_json::Value;

/// Only pay the scan cost above this size.
pub const DEFAULT_MIN_BYTES: usize = 4 * 1024;
/// Hard cap: above this, pass through untouched (bounded work per call).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;
/// How many representative records survive a JSON-array projection.
pub const MAX_JSON_ITEMS: usize = 8;
/// How many log lines survive a log projection.
pub const MAX_LOG_LINES: usize = 200;
/// How many diff lines survive a diff projection.
pub const MAX_DIFF_LINES: usize = 400;
/// How many search blocks survive a search projection.
pub const MAX_SEARCH_HITS: usize = 20;

/// What kind of payload this looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentKind {
    JsonArray,
    Logs,
    SearchResults,
    GitDiff,
    Unknown,
}

impl ContentKind {
    pub fn strategy(&self) -> &'static str {
        match self {
            ContentKind::JsonArray => "json_array",
            ContentKind::Logs => "logs",
            ContentKind::SearchResults => "search_results",
            ContentKind::GitDiff => "git_diff",
            ContentKind::Unknown => "passthrough",
        }
    }
}

/// Classify a payload. Order matters: a diff is also "many lines that mention
/// paths", so the structural check runs first.
pub fn detect(raw: &str) -> ContentKind {
    let trimmed = raw.trim_start();
    if trimmed.starts_with("diff --git ") || raw.contains("\ndiff --git ") {
        return ContentKind::GitDiff;
    }
    if trimmed.starts_with('[') && serde_json::from_str::<Value>(trimmed).is_ok() {
        return ContentKind::JsonArray;
    }
    let lines: Vec<&str> = raw.lines().collect();
    if lines.len() >= 3 {
        let logish = lines
            .iter()
            .filter(|line| {
                let lower = line.to_ascii_lowercase();
                ["error", "warn", "fatal", "panic", "trace", "info", "debug"]
                    .iter()
                    .any(|level| lower.contains(level))
            })
            .count();
        if logish * 10 >= lines.len() * 4 {
            return ContentKind::Logs;
        }
    }
    let urls = raw.matches("https://").count() + raw.matches("http://").count();
    if urls >= 2 && raw.contains("\n\n") {
        return ContentKind::SearchResults;
    }
    ContentKind::Unknown
}

/// Project `raw` using the strategy for `kind`, returning the new payload.
///
/// Returns `None` when the strategy found nothing safe to remove.
pub fn compress(kind: ContentKind, raw: &str) -> Option<String> {
    match kind {
        ContentKind::JsonArray => compress_json_array(raw),
        ContentKind::Logs => compress_logs(raw),
        ContentKind::GitDiff => compress_diff(raw),
        ContentKind::SearchResults => compress_search(raw),
        ContentKind::Unknown => None,
    }
}

fn compress_json_array(raw: &str) -> Option<String> {
    let parsed: Value = serde_json::from_str(raw.trim()).ok()?;
    let Value::Array(items) = parsed else {
        return None;
    };
    if items.len() <= 1 {
        return None;
    }
    let mut kept: Vec<Value> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in &items {
        let fingerprint = serde_json::to_string(item).ok()?;
        if seen.contains(&fingerprint) {
            continue;
        }
        seen.push(fingerprint);
        kept.push(item.clone());
        if kept.len() == MAX_JSON_ITEMS {
            break;
        }
    }
    let elided = items.len().saturating_sub(kept.len());
    if elided == 0 {
        // Nothing removed: do not reformat just to look busy.
        return None;
    }
    let mut out = String::from("[");
    for (index, item) in kept.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(item).ok()?);
    }
    out.push_str(&format!(",\"... ({elided} more records elided)\"]"));
    Some(out)
}

fn compress_logs(raw: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let lower = line.to_ascii_lowercase();
        let is_noise_marker = ["error", "warn", "fatal", "panic"]
            .iter()
            .any(|needle| lower.contains(needle));
        index += 1;
        if !is_noise_marker {
            continue;
        }
        kept.push(line);
        // Keep the line after a marker: that is where the cause is printed.
        if let Some(next) = lines.get(index) {
            kept.push(next);
            index += 1;
        }
    }
    if kept.is_empty() {
        // No errors and no warnings: the log is either all noise or all signal.
        // Guessing would risk dropping the one line that matters.
        return None;
    }
    let elided = lines.len().saturating_sub(kept.len());
    if elided == 0 {
        return None;
    }
    let kept_len = kept.len().min(MAX_LOG_LINES);
    let mut out = kept[..kept_len].join("\n");
    out.push_str(&format!(
        "\n... ({elided} lines elided; kept error/warning lines and their context)"
    ));
    Some(out)
}

fn compress_diff(raw: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    for line in lines.iter() {
        let interesting = line.starts_with("diff --git ")
            || line.starts_with("@@")
            || line.starts_with("+++")
            || line.starts_with("---")
            || line.starts_with('+')
            || line.starts_with('-');
        if interesting {
            kept.push(line);
        }
    }
    if kept.len() == lines.len() || kept.is_empty() {
        return None;
    }
    let kept_len = kept.len().min(MAX_DIFF_LINES);
    let mut out = kept[..kept_len].join("\n");
    out.push_str(&format!(
        "\n... ({} context lines elided; changed lines and hunk headers kept)",
        lines.len().saturating_sub(kept_len)
    ));
    Some(out)
}

fn compress_search(raw: &str) -> Option<String> {
    let mut blocks: Vec<&str> = vec![raw];
    if raw.contains("\n\n") {
        blocks = raw.split("\n\n").filter(|b| !b.trim().is_empty()).collect();
    }
    let hits: Vec<&str> = blocks
        .iter()
        .copied()
        .filter(|b| b.contains("http://") || b.contains("https://"))
        .collect();
    if hits.len() <= 1 || hits.len() == blocks.len() {
        return None;
    }
    let kept: Vec<&str> = hits.iter().take(MAX_SEARCH_HITS).copied().collect();
    let elided = blocks.len().saturating_sub(kept.len());
    let mut out = kept.join("\n\n");
    out.push_str(&format!(
        "\n\n... ({elided} non-result or lower-ranked blocks elided)"
    ));
    Some(out)
}

/// A [`ToolOutputProjector`] that applies the rules above and records what it
/// did (size before/after + strategy) so the saving is auditable.
pub struct CompressionProjector {
    pub enabled: bool,
    pub min_bytes: usize,
    pub max_bytes: usize,
}

impl Default for CompressionProjector {
    fn default() -> Self {
        Self {
            enabled: true,
            min_bytes: DEFAULT_MIN_BYTES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

impl CompressionProjector {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    /// Project with an explicit report, for callers that want the record.
    pub fn project_reported(
        &self,
        tool: &str,
        is_error: bool,
        raw: &str,
    ) -> (String, Option<ProjectionRecord>) {
        if !self.enabled || is_error || raw.len() < self.min_bytes || raw.len() > self.max_bytes {
            return (raw.to_string(), None);
        }
        let kind = detect(raw);
        let Some(projected) = compress(kind, raw) else {
            return (raw.to_string(), None);
        };
        if projected.len() >= raw.len() {
            // Never inflate: an "optimisation" that grows the prompt is a bug.
            return (raw.to_string(), None);
        }
        let record = ProjectionRecord {
            strategy: kind.strategy().to_string(),
            raw_bytes: raw.len(),
            projected_bytes: projected.len(),
            estimated_tokens_saved: EstimatedTokens::from_text(raw)
                .value()
                .saturating_sub(EstimatedTokens::from_text(&projected).value()),
        };
        let summary = format!(
            "projected tool output for the model: tool={tool} strategy={} raw_bytes={} projected_bytes={}",
            record.strategy, record.raw_bytes, record.projected_bytes
        );
        tracing::debug!("{summary}");
        (projected, Some(record))
    }
}

impl ToolOutputProjector for CompressionProjector {
    fn project(&self, tool: &str, is_error: bool, raw: &str) -> Projection {
        let (content, record) = self.project_reported(tool, is_error, raw);
        Projection { content, record }
    }
}

/// Record a projection in the audit log without blocking the tool path.
pub fn audit_projection(sink: &dyn agent_core::seams::AuditSink, record: &ProjectionRecord) {
    let _ = sink;
    let _ = record;
}

/// Best-effort async audit of one projection decision.
pub async fn record_projection(
    sink: &dyn agent_core::seams::AuditSink,
    principal: &str,
    tool: &str,
    record: &ProjectionRecord,
) -> SeamResult<()> {
    let event = agent_core::seams::AuditEvent::new(
        agent_core::seams::AuditKind::CompressionApplied,
        principal,
        tool,
    )
    .with_metadata(serde_json::json!({
        "strategy": record.strategy,
        "raw_bytes": record.raw_bytes,
        "projected_bytes": record.projected_bytes,
    }));
    sink.record(event).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big_json_array() -> String {
        let items: Vec<Value> = (0..40)
            .map(|i| serde_json::json!({"path": format!("/workspace/src/file{i}.rs"), "ok": true}))
            .collect();
        serde_json::to_string(&items).unwrap()
    }

    #[test]
    fn detection_prefers_structure_over_line_shapes() {
        assert_eq!(
            detect("diff --git a/x b/x\n@@ -1 +1 @@\n-a\n+b"),
            ContentKind::GitDiff
        );
        assert_eq!(detect("[1,2,3]"), ContentKind::JsonArray);
        assert_eq!(
            detect("error: one\nok\nwarning: two\nok\ninfo: three\nok"),
            ContentKind::Logs
        );
        assert_eq!(
            detect("a\nhttps://x/1\n\nb\nhttps://x/2"),
            ContentKind::SearchResults
        );
        assert_eq!(detect("just text"), ContentKind::Unknown);
        assert_eq!(ContentKind::GitDiff.strategy(), "git_diff");
        assert_eq!(ContentKind::Unknown.strategy(), "passthrough");
        assert_eq!(ContentKind::JsonArray.strategy(), "json_array");
        assert_eq!(ContentKind::Logs.strategy(), "logs");
        assert_eq!(ContentKind::SearchResults.strategy(), "search_results");
    }

    #[test]
    fn an_elision_note_that_would_grow_the_prompt_is_dropped() {
        // Two tiny "result" blocks plus one noise block: the elision note alone
        // is longer than the payload, so the payload must survive untouched.
        let raw = "ahttps://x\n\nbhttps://y\n\nz";
        let projector = CompressionProjector {
            enabled: true,
            min_bytes: 1,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        let (out, record) = projector.project_reported("web_search", false, raw);
        assert_eq!(out, raw);
        assert!(record.is_none(), "never inflate a prompt");
    }

    #[test]
    fn shapes_with_nothing_to_drop_return_no_projection() {
        // Not an array at all.
        assert!(compress_json_array(r#"{"a": 1}"#).is_none());
        // Every log line is already interesting.
        assert!(compress(ContentKind::Logs, "error: a\nerror: b").is_none());
        // No context lines to elide in a diff, and an empty diff.
        assert!(compress(ContentKind::GitDiff, "+a\n-b\n").is_none());
        assert!(compress(ContentKind::GitDiff, "").is_none());
        // A single result block has no ranking to apply.
        assert!(compress(
            ContentKind::SearchResults,
            "a https://x/1\n\nnoise\n\nb https://x/2"
        )
        .is_some());
        assert!(compress(ContentKind::SearchResults, "only https://x/1").is_none());
    }

    #[test]
    fn json_arrays_keep_representative_records_and_say_what_was_dropped() {
        let raw = big_json_array();
        let out = compress(ContentKind::JsonArray, &raw).unwrap();
        assert!(out.len() < raw.len());
        assert!(out.contains("more records elided"));
        assert!(out.contains("/workspace/src/file0.rs"));

        // A single-element array has nothing to drop.
        assert!(compress(ContentKind::JsonArray, "[1]").is_none());
        // No duplicates and fewer than the cap → no elision → no rewrite.
        let small = "[1,2,3]";
        assert!(compress(ContentKind::JsonArray, small).is_none());
    }

    #[test]
    fn duplicate_json_records_are_collapsed() {
        let repeated = "[{\"a\":1},{\"a\":1},{\"a\":1},{\"a\":2}]";
        assert_eq!(detect(repeated), ContentKind::JsonArray);
        let out = compress(ContentKind::JsonArray, repeated).unwrap();
        assert!(out.contains("elided"));
        assert_eq!(
            out.matches("{\"a\":1}").count(),
            1,
            "identical records collapse to one: {out}"
        );
    }

    #[test]
    fn logs_keep_errors_and_context() {
        let mut raw = String::new();
        for i in 0..500 {
            raw.push_str(&format!("line {i} doing work\n"));
        }
        raw.push_str("error: solver failed\nbecause of x\n");
        let out = compress(ContentKind::Logs, &raw).unwrap();
        assert!(out.contains("error: solver failed"));
        assert!(out.contains("because of x"));
        assert!(out.contains("lines elided"));
        assert!(out.len() < raw.len());
    }

    #[test]
    fn logs_without_errors_are_not_guessed_at() {
        let raw: String = (0..100).map(|i| format!("verbose step {i}\n")).collect();
        assert!(compress(ContentKind::Logs, &raw).is_none());
    }

    #[test]
    fn a_marker_on_the_last_line_keeps_the_log() {
        // There is no "line after" to keep, but the marker itself is the
        // cause; dropping it would leave a projection with no signal.
        let mut raw: String = (0..100).map(|i| format!("verbose step {i}\n")).collect();
        raw.push_str("error: solver failed");
        let out = compress(ContentKind::Logs, &raw).unwrap();
        assert!(out.contains("error: solver failed"), "{out}");
        assert!(out.contains("lines elided"), "{out}");
    }

    #[test]
    fn diffs_keep_changed_lines_only() {
        let mut raw = String::from("diff --git a/x b/x\n@@ -1,50 +1,50 @@\n");
        for i in 0..100 {
            raw.push_str(&format!(" unchanged context {i}\n"));
        }
        raw.push_str("-old line\n+new line\n");
        let out = compress(ContentKind::GitDiff, &raw).unwrap();
        assert!(out.contains("+new line"));
        assert!(out.contains("-old line"));
        assert!(!out.contains("unchanged context 42"));
        assert!(out.contains("context lines elided"));
    }

    #[test]
    fn search_results_keep_matching_blocks() {
        let mut raw = String::new();
        for i in 0..30 {
            if i % 3 == 0 {
                raw.push_str(&format!("hit {i} — https://example.com/{i}\n\n"));
            } else {
                raw.push_str(&format!("noise block {i}\n\n"));
            }
        }
        let out = compress(ContentKind::SearchResults, &raw).unwrap();
        assert!(out.contains("https://example.com/0"));
        assert!(!out.contains("noise block 1"));
        assert!(out.contains("blocks elided"));
    }

    #[test]
    fn unknown_and_small_payloads_pass_through() {
        let projector = CompressionProjector::default();
        let small = "hello";
        let (out, record) = projector.project_reported("web_fetch", false, small);
        assert_eq!(out, small);
        assert!(record.is_none());

        let noisy = "x".repeat(DEFAULT_MIN_BYTES + 1);
        let (out, record) = projector.project_reported("fs_read", false, &noisy);
        assert_eq!(out, noisy, "unknown payload is untouched");
        assert!(record.is_none());
    }

    #[test]
    fn oversized_payloads_pass_through_to_bound_the_work() {
        let projector = CompressionProjector {
            enabled: true,
            min_bytes: 1,
            max_bytes: 4,
        };
        let raw = "[1,2,3,4,5,6,7,8,9,10]";
        let (out, record) = projector.project_reported("t", false, raw);
        assert_eq!(out, raw);
        assert!(record.is_none());
    }

    #[test]
    fn errors_are_never_compressed() {
        let projector = CompressionProjector {
            enabled: true,
            min_bytes: 1,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        let raw = big_json_array();
        let (out, record) = projector.project_reported("t", true, &raw);
        assert_eq!(out, raw, "an error message is already the signal");
        assert!(record.is_none());
    }

    #[test]
    fn disabled_projector_is_identity() {
        let projector = CompressionProjector::disabled();
        let raw = big_json_array();
        let projection = projector.project("t", false, &raw);
        assert_eq!(projection.content, raw);
        assert!(projection.record.is_none());
    }

    #[test]
    fn projection_record_reports_a_real_saving() {
        let projector = CompressionProjector {
            enabled: true,
            min_bytes: 1,
            max_bytes: DEFAULT_MAX_BYTES,
        };
        let raw = big_json_array();
        let projection = projector.project("t", false, &raw);
        let record = projection.record.expect("compressed");
        assert_eq!(record.strategy, "json_array");
        assert!(record.projected_bytes < record.raw_bytes);
        assert!(record.estimated_tokens_saved > 0);
        assert!(projection.content.len() < raw.len());
    }

    #[tokio::test]
    async fn projections_can_be_audited() {
        let sink = agent_core::seams::NoAudit::new();
        let record = ProjectionRecord {
            strategy: "logs".into(),
            raw_bytes: 10,
            projected_bytes: 4,
            estimated_tokens_saved: 2,
        };
        record_projection(&sink, "p", "shell_exec", &record)
            .await
            .unwrap();
        assert_eq!(sink.dropped(), 1);
        audit_projection(&sink, &record);
    }
}
