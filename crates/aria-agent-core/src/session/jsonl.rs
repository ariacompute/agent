//! JSONL session persistence (one artifact per session).
//!
//! Mirrors dsh's `session-persistence-jsonl`: each session is a single
//! append-only file, one JSON record per line. A corrupted line is skipped with a
//! warning instead of poisoning the whole history — a partially written tail is
//! the normal crash outcome.

use super::{SessionError, SessionPersistence, SessionRecord};
use async_trait::async_trait;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Directory-backed JSONL provider.
#[derive(Debug, Clone)]
pub struct JsonlPersistence {
    root: PathBuf,
}

impl JsonlPersistence {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of a session's artifact. Session ids are sanitized so an id can never
    /// escape the directory.
    pub fn path_for(&self, session: &str) -> PathBuf {
        let mut safe: String = session
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        if safe.is_empty() {
            safe = "session".into();
        }
        self.root.join(format!("{safe}.jsonl"))
    }
}

#[async_trait]
impl SessionPersistence for JsonlPersistence {
    async fn append(&self, session: &str, records: &[SessionRecord]) -> super::SessionResult<()> {
        if records.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(&self.root)
            .map_err(|e| SessionError::Io(format!("mkdir {}: {e}", self.root.display())))?;
        let path = self.path_for(session);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| SessionError::Io(format!("open {}: {e}", path.display())))?;
        for record in records {
            let line = serde_json::to_string(record)
                .map_err(|e| SessionError::Serde(format!("encode record: {e}")))?;
            file.write_all(line.as_bytes())
                .and_then(|_| file.write_all(b"\n"))
                .map_err(|e| SessionError::Io(format!("append {}: {e}", path.display())))?;
        }
        file.flush()
            .map_err(|e| SessionError::Io(format!("flush {}: {e}", path.display())))?;
        Ok(())
    }

    async fn read(&self, session: &str, from_seq: u64) -> super::SessionResult<Vec<SessionRecord>> {
        let path = self.path_for(session);
        let raw = match std::fs::read_to_string(&path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SessionError::NotFound(session.to_string()))
            }
            Err(e) => return Err(SessionError::Io(format!("read {}: {e}", path.display()))),
        };
        let mut out = Vec::new();
        for (lineno, line) in raw.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<SessionRecord>(line) {
                Ok(record) if record.seq >= from_seq => out.push(record),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!(
                        session = %session,
                        line = lineno + 1,
                        error = %e,
                        "skipping corrupted session record"
                    );
                }
            }
        }
        out.sort_by_key(|r| r.seq);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::SessionEvent;

    fn record(seq: u64, text: &str) -> SessionRecord {
        SessionRecord {
            seq,
            at_ms: 1,
            event: SessionEvent::TextDelta { text: text.into() },
        }
    }

    #[tokio::test]
    async fn append_then_read_roundtrip_and_from_seq() {
        let dir = tempfile::tempdir().unwrap();
        let p = JsonlPersistence::new(dir.path());
        p.append("s1", &[record(0, "a"), record(1, "b")])
            .await
            .unwrap();
        p.append("s1", &[record(2, "c")]).await.unwrap();
        let all = p.read("s1", 0).await.unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(all[2].event.kind(), "text_delta");
        let tail = p.read("s1", 2).await.unwrap();
        assert_eq!(tail.len(), 1);
        assert_eq!(tail[0].seq, 2);
    }

    #[tokio::test]
    async fn missing_and_empty_sessions_behave() {
        let dir = tempfile::tempdir().unwrap();
        let p = JsonlPersistence::new(dir.path());
        assert!(matches!(
            p.read("nope", 0).await,
            Err(SessionError::NotFound(_))
        ));
        p.append("empty", &[]).await.unwrap();
        assert!(matches!(
            p.read("empty", 0).await,
            Err(SessionError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn corrupted_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let p = JsonlPersistence::new(dir.path());
        p.append("s", &[record(0, "ok")]).await.unwrap();
        let path = p.path_for("s");
        let mut raw = std::fs::read_to_string(&path).unwrap();
        raw.push_str("{not json\n");
        std::fs::write(&path, raw).unwrap();
        p.append("s", &[record(1, "after")]).await.unwrap();
        let read = p.read("s", 0).await.unwrap();
        assert_eq!(read.len(), 2);
        assert_eq!(read[1].seq, 1);
    }

    #[test]
    fn session_ids_cannot_escape_the_directory() {
        let p = JsonlPersistence::new("/tmp/x");
        let path = p.path_for("../../etc/passwd");
        assert!(path.starts_with("/tmp/x"));
        assert!(!path.to_string_lossy().contains(".."));
        assert_eq!(p.path_for(""), PathBuf::from("/tmp/x/session.jsonl"));
        assert_eq!(p.path_for("a-b_c"), PathBuf::from("/tmp/x/a-b_c.jsonl"));
        assert_eq!(p.root(), Path::new("/tmp/x"));
    }
}
