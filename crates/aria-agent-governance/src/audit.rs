//! Append-only JSONL audit sink with retention and NDJSON export.
//!
//! This is the "make audit thick" half of the plan (ADR-0021). Runta advertises
//! recording as one of three pillars and then documents almost nothing about it;
//! here the contract is explicit:
//!
//! * one JSON object per line, schema = [`agent_core::seams::AuditEvent`];
//! * files are `audit-YYYYMMDD.jsonl`, so retention and export are trivial;
//! * payloads are sanitised before writing — a nested `authorization` or `token`
//!   key is replaced with `[redacted]`, because audit must never become the place
//!   secrets leak;
//! * `recent()` reads from a bounded in-memory ring for the API, not from disk.

use agent_core::seams::{now_ms, AuditEvent, AuditKind, AuditSink, SeamError, SeamResult};
use async_trait::async_trait;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use tokio::sync::Mutex;

/// How many events stay hot in memory for `recent()`.
pub const DEFAULT_RECENT_CAPACITY: usize = 512;

const REDACTED: &str = "[redacted]";
const FORBIDDEN_KEYS: &[&str] = &[
    "authorization",
    "api_key",
    "apikey",
    "token",
    "secret",
    "password",
    "credential",
    "cookie",
];

/// Sanitise one event: drop/replace any secret-bearing metadata key.
///
/// Recurses into arrays and objects because tool arguments are nested.
pub fn sanitize(mut event: AuditEvent) -> AuditEvent {
    event.metadata = sanitize_value(event.metadata);
    event
}

fn sanitize_value(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (key, value) in map {
                let lower = key.to_ascii_lowercase();
                if FORBIDDEN_KEYS.iter().any(|k| lower.contains(k)) {
                    out.insert(key, serde_json::Value::String(REDACTED.into()));
                } else {
                    out.insert(key, sanitize_value(value));
                }
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(sanitize_value).collect())
        }
        other => other,
    }
}

/// A JSONL file sink.
pub struct JsonlAuditSink {
    dir: PathBuf,
    retention_days: u64,
    recent: Mutex<VecDeque<AuditEvent>>,
    capacity: usize,
}

impl std::fmt::Debug for JsonlAuditSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlAuditSink")
            .field("dir", &self.dir)
            .field("retention_days", &self.retention_days)
            .finish_non_exhaustive()
    }
}

impl JsonlAuditSink {
    pub fn new(dir: impl AsRef<Path>, retention_days: u64) -> SeamResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)
            .map_err(|e| SeamError::Io(format!("audit dir {}: {e}", dir.display())))?;
        let sink = Self {
            dir,
            retention_days,
            recent: Mutex::new(VecDeque::with_capacity(DEFAULT_RECENT_CAPACITY)),
            capacity: DEFAULT_RECENT_CAPACITY,
        };
        sink.purge_expired(now_ms())?;
        Ok(sink)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Delete files older than the retention window. Returns how many went away.
    pub fn purge_expired(&self, now: u64) -> SeamResult<usize> {
        if self.retention_days == 0 {
            return Ok(0);
        }
        let cutoff = now.saturating_sub(self.retention_days.saturating_mul(86_400_000));
        let cutoff_day = cutoff / 86_400_000;
        let mut removed = 0;
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => {
                return Err(SeamError::Io(format!(
                    "read audit dir {}: {e}",
                    self.dir.display()
                )))
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(day) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(day_from_filename)
            else {
                continue;
            };
            if day < cutoff_day && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Every persisted line for one principal, oldest first.
    pub fn export(&self, principal_id: &str) -> SeamResult<Vec<AuditEvent>> {
        let mut out = Vec::new();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&self.dir)
            .map_err(|e| SeamError::Io(format!("read audit dir: {e}")))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("audit-") && n.ends_with(".jsonl"))
                    .unwrap_or(false)
            })
            .collect();
        files.sort();
        for file in files {
            let raw = std::fs::read_to_string(&file)
                .map_err(|e| SeamError::Io(format!("read {}: {e}", file.display())))?;
            for line in raw.lines().filter(|l| !l.trim().is_empty()) {
                match serde_json::from_str::<AuditEvent>(line) {
                    Ok(event) if event.principal_id == principal_id => out.push(event),
                    Ok(_) => {}
                    Err(e) => {
                        return Err(SeamError::Invalid(format!(
                            "corrupt audit line in {}: {e}",
                            file.display()
                        )))
                    }
                }
            }
        }
        Ok(out)
    }

    fn path_for(&self, ts_ms: u64) -> PathBuf {
        self.dir
            .join(format!("audit-{}.jsonl", ymd_from_days(ts_ms / 86_400_000)))
    }
}

#[async_trait]
impl AuditSink for JsonlAuditSink {
    async fn record(&self, event: AuditEvent) -> SeamResult<()> {
        let event = sanitize(event);
        let path = self.path_for(event.ts_ms);
        let line = serde_json::to_string(&event)
            .expect("audit events are plain structs and always serialise");
        let write_path = path.clone();
        tokio::task::spawn_blocking(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&write_path)?;
            file.write_all(line.as_bytes())?;
            file.write_all(b"\n")
        })
        .await
        .expect("the audit append task does not panic")
        .map_err(|e| SeamError::Io(format!("append {}: {e}", path.display())))?;

        let mut recent = self.recent.lock().await;
        if recent.len() == self.capacity {
            recent.pop_front();
        }
        recent.push_back(event);
        Ok(())
    }

    async fn recent(&self, limit: usize) -> SeamResult<Vec<AuditEvent>> {
        let recent = self.recent.lock().await;
        let take = limit.min(recent.len());
        Ok(recent.iter().skip(recent.len() - take).cloned().collect())
    }
}

fn day_from_filename(name: &str) -> Option<u64> {
    let rest = name.strip_prefix("audit-")?.strip_suffix(".jsonl")?;
    let mut parts = rest.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let m: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(days_from_civil(y, m, d))
}

/// Howard Hinnant's civil-from-days / days-from-civil (no chrono dependency).
fn ymd_from_days(days: u64) -> String {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn days_from_civil(y: i64, m: u32, d: u32) -> u64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468).max(0) as u64
}

/// A ready-to-write `AuditKind` for a denied egress decision.
pub fn denied(kind: AuditKind, principal: &str, target: &str) -> AuditEvent {
    AuditEvent::new(kind, principal, target).with_decision(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(principal: &str, target: &str) -> AuditEvent {
        AuditEvent::new(AuditKind::EgressDecision, principal, target)
            .with_decision(false)
            .with_metadata(json!({"reason": "not on allowlist"}))
    }

    #[test]
    fn calendar_helpers_round_trip() {
        assert_eq!(ymd_from_days(0), "1970-01-01");
        assert_eq!(ymd_from_days(20_000), "2024-10-04");
        assert_eq!(day_from_filename("audit-1970-01-01.jsonl"), Some(0));
        assert_eq!(day_from_filename("audit-2024-10-04.jsonl"), Some(20_000));
        assert_eq!(day_from_filename("other-2024-10-04.jsonl"), None);
        assert_eq!(day_from_filename("audit-2024-10.jsonl"), None);
        assert_eq!(day_from_filename("audit-x-y-z.jsonl"), None);
        // Extra path segments are not a day file.
        assert_eq!(day_from_filename("audit-2024-10-04-extra.jsonl"), None);
        assert_eq!(days_from_civil(1970, 1, 1), 0);
    }

    #[test]
    fn sanitize_redacts_secret_bearing_keys_recursively() {
        let event = event("p", "api.openai.com").with_metadata(json!({
            "authorization": "Bearer sk-1",
            "nested": {"api_key": "sk-2", "safe": 1},
            "list": [{"Secret": "v"}, 3],
            "keep": "ok"
        }));
        let clean = sanitize(event);
        let text = serde_json::to_string(&clean).unwrap();
        assert!(!text.contains("sk-1"));
        assert!(!text.contains("sk-2"));
        assert!(!text.contains("\"v\""));
        assert!(text.contains("[redacted]"));
        assert!(text.contains("\"keep\":\"ok\""));
        assert!(text.contains("\"safe\":1"));
    }

    #[tokio::test]
    async fn events_are_persisted_and_readable() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 30).unwrap();
        sink.record(event("p", "a.com")).await.unwrap();
        sink.record(event("p", "b.com")).await.unwrap();
        sink.record(event("other", "c.com")).await.unwrap();

        let recent = sink.recent(2).await.unwrap();
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[1].target, "c.com");

        assert!(sink.recent(0).await.unwrap().is_empty());
        assert!(sink.recent(99).await.unwrap().len() == 3);

        let exported = sink.export("p").unwrap();
        assert_eq!(exported.len(), 2);
        assert_eq!(exported[0].target, "a.com");
        assert!(sink.export("nobody").unwrap().is_empty());
    }

    #[tokio::test]
    async fn audit_lines_are_ndjson_and_sanitised_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 30).unwrap();
        let event = event("p", "a.com").with_metadata(json!({"token": "sk-leak"}));
        sink.record(event).await.unwrap();
        let files: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .collect();
        assert_eq!(files.len(), 1);
        let raw = std::fs::read_to_string(&files[0]).unwrap();
        assert_eq!(raw.lines().count(), 1);
        assert!(!raw.contains("sk-leak"));
        assert!(raw.contains("[redacted]"));
    }

    #[tokio::test]
    async fn retention_removes_old_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("audit-1970-01-01.jsonl");
        let fresh = dir.path().join(format!(
            "audit-{}.jsonl",
            ymd_from_days(now_ms() / 86_400_000)
        ));
        std::fs::write(&old, "{}\n").unwrap();
        std::fs::write(&fresh, "{}\n").unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 7).unwrap();
        assert!(!old.exists(), "expired file removed");
        assert!(fresh.exists());
        assert_eq!(sink.purge_expired(now_ms()).unwrap(), 0);
    }

    #[tokio::test]
    async fn retention_zero_keeps_everything() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("audit-1970-01-01.jsonl");
        std::fs::write(&old, "{}\n").unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 0).unwrap();
        assert!(old.exists());
        assert_eq!(sink.purge_expired(now_ms()).unwrap(), 0);
    }

    #[tokio::test]
    async fn a_corrupt_line_is_reported_on_export() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 30).unwrap();
        sink.record(event("p", "a.com")).await.unwrap();
        let file = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .next()
            .unwrap()
            .path();
        let mut raw = std::fs::read_to_string(&file).unwrap();
        raw.push_str("{broken\n");
        std::fs::write(&file, raw).unwrap();
        assert!(matches!(sink.export("p"), Err(SeamError::Invalid(_))));
    }

    #[tokio::test]
    async fn the_ring_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 30).unwrap();
        for i in 0..(DEFAULT_RECENT_CAPACITY + 10) {
            sink.record(event("p", &format!("h{i}"))).await.unwrap();
        }
        let recent = sink.recent(usize::MAX).await.unwrap();
        assert_eq!(recent.len(), DEFAULT_RECENT_CAPACITY);
        assert_eq!(
            recent.last().unwrap().target,
            format!("h{}", DEFAULT_RECENT_CAPACITY + 9)
        );
    }

    #[tokio::test]
    async fn a_missing_dir_is_created_and_a_bad_path_reported() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("a/b/c");
        let sink = JsonlAuditSink::new(&nested, 30).unwrap();
        assert_eq!(sink.dir(), nested);
        assert!(nested.exists());

        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "x").unwrap();
        assert!(JsonlAuditSink::new(&file, 30).is_err());
    }

    #[test]
    fn denied_helper_sets_the_decision() {
        let event = denied(AuditKind::QuotaDecision, "p", "sandbox-1");
        assert_eq!(event.decision, Some(false));
    }

    #[tokio::test]
    async fn export_and_purge_skip_what_they_cannot_use() {
        let dir = tempfile::tempdir().unwrap();
        let sink = JsonlAuditSink::new(dir.path(), 30).unwrap();
        assert!(format!("{sink:?}").contains("JsonlAuditSink"));
        sink.record(event("p", "shell")).await.unwrap();
        assert_eq!(sink.export("p").unwrap().len(), 1);
        assert!(sink.export("nobody").unwrap().is_empty());
        assert_eq!(sink.recent(0).await.unwrap().len(), 0);

        // An old day file is purged; an unrelated file is skipped, not deleted.
        let old = dir.path().join("audit-2000-01-01.jsonl");
        std::fs::write(&old, "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        assert_eq!(sink.purge_expired(now_ms()).unwrap(), 1);
        assert!(!old.exists());
        assert!(dir.path().join("notes.txt").exists());

        // A day file that exists but cannot be read is a reported error.
        std::fs::create_dir_all(dir.path().join("audit-1999-01-01.jsonl")).unwrap();
        assert!(sink.export("p").is_err());
    }

    #[test]
    fn purge_removes_old_days_and_survives_a_missing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("audit-2000-01-01.jsonl");
        std::fs::write(&old, "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        // `new` purges immediately: the old day goes, the unrelated file stays.
        let sink = JsonlAuditSink::new(dir.path(), 1).unwrap();
        assert!(!old.exists());
        assert!(dir.path().join("notes.txt").exists());
        assert_eq!(sink.purge_expired(now_ms()).unwrap(), 0);

        let nested = dir.path().join("gone");
        let sink = JsonlAuditSink::new(&nested, 1).unwrap();
        std::fs::remove_dir_all(&nested).unwrap();
        assert_eq!(sink.purge_expired(now_ms()).unwrap(), 0);
        assert!(sink.export("p").is_err(), "export reports the missing dir");

        // A path that is not a directory at all is an error, not "nothing to do".
        let file = dir.path().join("not-a-dir-2");
        std::fs::write(&file, "x").unwrap();
        let mut broken = JsonlAuditSink::new(dir.path(), 1).unwrap();
        broken.dir = file;
        assert!(broken.purge_expired(now_ms()).is_err());
    }

    #[tokio::test]
    async fn a_failed_append_is_reported_and_the_ring_wraps() {
        let dir = tempfile::tempdir().unwrap();
        let mut sink = JsonlAuditSink::new(dir.path(), 0).unwrap();
        sink.capacity = 1;
        sink.record(event("p", "a")).await.unwrap();
        sink.record(event("p", "b")).await.unwrap();
        let recent = sink.recent(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "b");

        // Make today's file a directory: the append must fail loudly.
        let blocker = sink.path_for(now_ms());
        std::fs::remove_file(&blocker).unwrap();
        std::fs::create_dir_all(&blocker).unwrap();
        assert!(matches!(
            sink.record(event("p", "c")).await,
            Err(SeamError::Io(_))
        ));
    }
}
