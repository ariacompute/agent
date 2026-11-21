//! Memory backends for the Rust/Swift/Kotlin client.
//!
//! * `cloud` — the agent-cloud session memory REST endpoints
//!   (`/v1/agents/sessions/{id}/memory`), same as `sdk/js` / `sdk/python`.
//! * `local` — **aria memo** (SQLite), reached through the `aria-memo` CLI
//!   (`--json`). The CLI is an injectable seam so tests never need the binary.
//! * `both` — writes to both and reads merged + deduped (the cloud copy wins).
//!
//! There is no embedded store any more: this crate is a cloud API client, so a
//! local copy needs the `aria-memo` binary on the host.

use std::process::Command;
use std::sync::Arc;

use serde_json::Value;

use crate::transport::{encode_segment, get_json, post_json, ResolvedClient};
use crate::AriaError;

/// Runs `aria-memo <args…>` and returns stdout.
pub(crate) trait MemoRunner: Send + Sync {
    fn run(&self, args: &[String]) -> Result<String, AriaError>;
}

/// Default runner: spawns the `aria-memo` CLI.
pub(crate) struct CliMemoRunner {
    bin: String,
}

impl CliMemoRunner {
    pub(crate) fn new(bin: impl Into<String>) -> Self {
        Self { bin: bin.into() }
    }
}

impl MemoRunner for CliMemoRunner {
    fn run(&self, args: &[String]) -> Result<String, AriaError> {
        let output = Command::new(&self.bin)
            .args(args)
            .output()
            .map_err(|e| AriaError::Config {
                detail: format!(
                    "cannot run `{}`: {e}. Install aria-memo (`aria-memo setup`) or set ARIA_MEMO_BIN",
                    self.bin
                ),
            })?;
        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).to_string())
        } else {
            Err(AriaError::Unknown {
                detail: format!(
                    "`{}` exited with {}: {}",
                    self.bin,
                    output
                        .status
                        .code()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "signal".into()),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            })
        }
    }
}

/// Value convention shared by every SDK: a keyed local memory is `key: value`.
pub(crate) fn encode_local(key: &str, value: &str) -> String {
    format!("{key}: {value}")
}

/// Strip the `key: ` prefix written by [`encode_local`]; other shapes pass through.
pub(crate) fn decode_local(key: &str, content: &str) -> String {
    let prefix = format!("{key}: ");
    content.strip_prefix(&prefix).unwrap_or(content).to_string()
}

/// Parse the CLI's `--json` output (a single object or an array of rows).
pub(crate) fn parse_rows(raw: &str) -> Vec<Value> {
    let text = raw.trim();
    if text.is_empty() {
        return Vec::new();
    }
    match serde_json::from_str::<Value>(text) {
        Ok(Value::Array(rows)) => rows,
        Ok(other) => vec![other],
        Err(_) => Vec::new(),
    }
}

/// A session's keyed memory, resolved against its configured backend.
pub(crate) struct Memory {
    backend: String,
    session_id: String,
    client: ResolvedClient,
    memo_bin: String,
    memo_db: String,
    runner: Option<Arc<dyn MemoRunner>>,
}

impl Memory {
    pub(crate) fn new(
        backend: impl Into<String>,
        session_id: impl Into<String>,
        client: ResolvedClient,
        config: Option<&crate::MemoryConfig>,
        runner: Option<Arc<dyn MemoRunner>>,
    ) -> Result<Self, AriaError> {
        let backend = backend.into();
        let backend = if backend.trim().is_empty() {
            "cloud".to_string()
        } else {
            backend.trim().to_string()
        };
        if !matches!(backend.as_str(), "cloud" | "local" | "both") {
            return Err(AriaError::Config {
                detail: format!("unknown memory backend: {backend} (expected cloud|local|both)"),
            });
        }
        Ok(Self {
            backend,
            session_id: session_id.into(),
            client,
            memo_bin: config
                .and_then(|c| c.memo_bin.clone())
                .or_else(|| env_non_empty("ARIA_MEMO_BIN"))
                .unwrap_or_else(|| "aria-memo".to_string()),
            memo_db: config
                .and_then(|c| c.memo_db.clone())
                .or_else(|| env_non_empty("ARIA_MEMO_DB"))
                .unwrap_or_else(|| "memo.db".to_string()),
            runner,
        })
    }

    #[cfg(test)]
    pub(crate) fn backend(&self) -> &str {
        &self.backend
    }

    /// Write one keyed value through the configured backend(s).
    pub(crate) fn put(&self, key: &str, value: &str) -> Result<(), AriaError> {
        match self.backend.as_str() {
            "local" => self.local_put(key, value),
            "both" => {
                let local = self.local_put(key, value);
                let cloud = self.cloud_put(key, value);
                match (local, cloud) {
                    (Err(l), Err(c)) => Err(AriaError::Unknown {
                        detail: format!("memory write failed on every backend: {l}; {c}"),
                    }),
                    _ => Ok(()),
                }
            }
            _ => self.cloud_put(key, value),
        }
    }

    /// Read one key (missing keys resolve to `null`).
    pub(crate) fn get(&self, key: &str) -> Result<Option<String>, AriaError> {
        match self.backend.as_str() {
            "local" => self.local_get(key),
            "both" => {
                let local = self.local_get(key);
                let cloud = self.cloud_get(key);
                if let Ok(Some(value)) = cloud {
                    return Ok(Some(value));
                }
                if let Ok(Some(value)) = local {
                    return Ok(Some(value));
                }
                match (local, cloud) {
                    (Err(l), Err(c)) => Err(AriaError::Unknown {
                        detail: format!("memory read failed on every backend: {l}; {c}"),
                    }),
                    _ => Ok(None),
                }
            }
            _ => self.cloud_get(key),
        }
    }

    fn memory_path(&self, key: Option<&str>) -> String {
        let base = format!(
            "/v1/agents/sessions/{}/memory",
            encode_segment(&self.session_id)
        );
        match key {
            Some(key) => format!("{base}/{}", encode_segment(key)),
            None => base,
        }
    }

    fn cloud_put(&self, key: &str, value: &str) -> Result<(), AriaError> {
        let body = serde_json::json!({ "key": key, "value": value, "kind": "long_term" });
        let _: Value = post_json(&self.client, &self.memory_path(None), &body)?;
        Ok(())
    }

    fn cloud_get(&self, key: &str) -> Result<Option<String>, AriaError> {
        #[derive(serde::Deserialize)]
        struct MemoryItem {
            content: Option<String>,
        }
        let res: MemoryItem = get_json(&self.client, &self.memory_path(Some(key)))?;
        Ok(res.content)
    }

    fn args(&self, command: &[&str]) -> Vec<String> {
        let mut args = vec!["--db".to_string(), self.memo_db.clone()];
        args.extend(command.iter().map(|a| a.to_string()));
        args
    }

    fn run_memo(&self, args: Vec<String>) -> Result<String, AriaError> {
        match &self.runner {
            Some(runner) => runner.run(&args),
            None => CliMemoRunner::new(self.memo_bin.clone()).run(&args),
        }
    }

    fn local_put(&self, key: &str, value: &str) -> Result<(), AriaError> {
        let content = encode_local(key, value);
        self.run_memo(self.args(&[
            "add",
            "--type",
            "long_term:semantic",
            "--content",
            &content,
            "--importance",
            "0.8",
        ]))?;
        Ok(())
    }

    fn local_get(&self, key: &str) -> Result<Option<String>, AriaError> {
        let raw = self.run_memo(self.args(&["search", "--text", key, "--top-k", "5", "--json"]))?;
        let rows = parse_rows(&raw);
        let prefix = format!("{key}: ");
        // Only a row that carries the `key: ` prefix is this key's memory; an
        // unrelated hit must never be returned as if it were the requested one.
        let hit = rows.iter().find(|r| {
            r.get("content")
                .and_then(Value::as_str)
                .is_some_and(|c| c.starts_with(&prefix))
        });
        Ok(hit
            .and_then(|r| r.get("content"))
            .and_then(Value::as_str)
            .map(|content| decode_local(key, content)))
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeMemo {
        calls: Mutex<Vec<Vec<String>>>,
        canned: Mutex<VecDeque<Result<String, String>>>,
    }

    impl MemoRunner for FakeMemo {
        fn run(&self, args: &[String]) -> Result<String, AriaError> {
            self.calls.lock().unwrap().push(args.to_vec());
            let next = self
                .canned
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(String::new()));
            next.map_err(|message| AriaError::Unknown { detail: message })
        }
    }

    /// Write a small executable that ignores its arguments.
    #[cfg(unix)]
    fn fake_bin(body: &str) -> (String, tempfile::TempDir) {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("aria-memo");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).expect("write");
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
        (path.to_string_lossy().to_string(), dir)
    }

    fn client() -> ResolvedClient {
        crate::transport::resolve_client(Some(&crate::ClientOptions {
            base_url: Some("http://127.0.0.1:1".into()),
            api_key: None,
            beta_header: None,
        }))
        .expect("client")
    }

    fn local_config(bin: String) -> crate::MemoryConfig {
        crate::MemoryConfig {
            backend: None,
            memo_bin: Some(bin),
            memo_db: Some("memories.db".into()),
        }
    }

    #[test]
    fn keyed_values_round_trip_through_the_prefix() {
        assert_eq!(encode_local("user", "Ada"), "user: Ada");
        assert_eq!(decode_local("user", "user: Ada"), "Ada");
        assert_eq!(decode_local("user", "someone else"), "someone else");
    }

    #[test]
    fn json_rows_accept_arrays_objects_and_garbage() {
        assert_eq!(parse_rows("[{\"content\":\"a\"}]").len(), 1);
        assert_eq!(parse_rows("{\"content\":\"a\"}").len(), 1);
        assert!(parse_rows("   ").is_empty());
        assert!(parse_rows("not json").is_empty());
    }

    #[test]
    fn an_unknown_backend_is_a_config_error() {
        let err = Memory::new("nowhere", "s", client(), None, None)
            .err()
            .expect("must fail");
        assert!(matches!(err, AriaError::Config { .. }), "{err}");
    }

    #[test]
    fn a_blank_backend_defaults_to_cloud() {
        let memory = Memory::new("  ", "s", client(), None, None).expect("memory");
        assert_eq!(memory.backend(), "cloud");
    }

    #[test]
    fn the_local_backend_drives_the_cli_and_reads_the_keyed_row() {
        let fake = Arc::new(FakeMemo::default());
        {
            let mut canned = fake.canned.lock().unwrap();
            canned.push_back(Ok(String::new())); // `add` returns nothing
            canned.push_back(Ok(r#"[{"content":"user: Ada"}]"#.into())); // `search`
        }
        let memory = Memory::new("local", "s", client(), None, Some(fake.clone())).expect("memory");
        memory.put("user", "Ada").expect("put");
        assert_eq!(memory.get("user").expect("get").as_deref(), Some("Ada"));

        let calls = fake.calls.lock().unwrap();
        assert_eq!(calls[0][0], "--db");
        assert!(calls[0].contains(&"add".to_string()));
        assert!(calls[0].contains(&"user: Ada".to_string()));
        assert!(calls[1].contains(&"search".to_string()));
        assert_eq!(calls[1].last().unwrap(), "--json");
    }

    #[test]
    fn the_local_backend_prefers_the_matching_row_and_allows_no_hits() {
        let fake = Arc::new(FakeMemo::default());
        {
            let mut canned = fake.canned.lock().unwrap();
            // FIFO: the matching row answers the first `get`, `[]` the second.
            canned.push_back(Ok(r#"[{"content":"other"},{"content":"user: Ada"}]"#.into()));
            canned.push_back(Ok("[]".into()));
        }
        let memory = Memory::new("local", "s", client(), None, Some(fake)).expect("memory");
        assert_eq!(memory.get("user").expect("first").as_deref(), Some("Ada"));
        assert_eq!(memory.get("user").expect("empty"), None);
    }

    #[test]
    fn a_row_without_the_key_prefix_is_not_returned() {
        let fake = Arc::new(FakeMemo::default());
        fake.canned
            .lock()
            .unwrap()
            .push_back(Ok(r#"[{"content":"someone else"}]"#.into()));
        let memory = Memory::new("local", "s", client(), None, Some(fake)).expect("memory");
        assert_eq!(memory.get("user").expect("get"), None);
    }

    #[test]
    fn local_failures_surface() {
        let fake = Arc::new(FakeMemo::default());
        fake.canned.lock().unwrap().push_back(Err("boom".into()));
        let memory = Memory::new("local", "s", client(), None, Some(fake)).expect("memory");
        let err = memory.put("user", "Ada").unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
    }

    #[test]
    fn the_default_runner_reports_a_missing_binary() {
        let memory = Memory::new(
            "local",
            "s",
            client(),
            Some(&local_config("aria-memo-does-not-exist".into())),
            None,
        )
        .expect("memory");
        let err = memory.put("user", "Ada").unwrap_err();
        assert!(err.to_string().contains("aria-memo"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_reports_a_non_zero_exit() {
        let (bin, _dir) = fake_bin("echo broken >&2; exit 3");
        let memory =
            Memory::new("local", "s", client(), Some(&local_config(bin)), None).expect("memory");
        let err = memory.put("user", "Ada").unwrap_err();
        assert!(err.to_string().contains("exited with 3"), "{err}");
        assert!(err.to_string().contains("broken"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_signalled_runner_is_reported() {
        let (bin, _dir) = fake_bin("kill -9 $$");
        let memory =
            Memory::new("local", "s", client(), Some(&local_config(bin)), None).expect("memory");
        let err = memory.put("user", "Ada").unwrap_err();
        assert!(err.to_string().contains("exited with signal"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn the_real_runner_reads_stdout() {
        let (bin, _dir) = fake_bin(r#"printf '[{"content":"user: Ada"}]'"#);
        let memory =
            Memory::new("local", "s", client(), Some(&local_config(bin)), None).expect("memory");
        assert_eq!(memory.get("user").expect("get").as_deref(), Some("Ada"));
    }

    #[test]
    fn both_writes_to_both_and_reads_fall_back_to_local() {
        let fake = Arc::new(FakeMemo::default());
        {
            let mut canned = fake.canned.lock().unwrap();
            canned.push_back(Ok(String::new())); // `put` (local write)
            canned.push_back(Ok(r#"[{"content":"user: Local"}]"#.into())); // `get`
        }
        // The cloud side is unreachable, so `both` still answers from local.
        let memory = Memory::new("both", "s", client(), None, Some(fake)).expect("memory");
        memory.put("user", "Ada").expect("one backend succeeded");
        assert_eq!(memory.get("user").expect("get").as_deref(), Some("Local"));
    }

    #[test]
    fn both_returns_none_when_only_one_side_is_empty() {
        let fake = Arc::new(FakeMemo::default());
        {
            let mut canned = fake.canned.lock().unwrap();
            canned.push_back(Ok(String::new()));
            canned.push_back(Ok("[]".into()));
        }
        let memory = Memory::new("both", "s", client(), None, Some(fake)).expect("memory");
        assert_eq!(memory.get("user").expect("get"), None);
    }

    #[test]
    fn both_fails_only_when_every_backend_fails() {
        let fake = Arc::new(FakeMemo::default());
        {
            let mut canned = fake.canned.lock().unwrap();
            canned.push_back(Err("local down".into()));
            canned.push_back(Err("local down".into()));
        }
        let memory = Memory::new("both", "s", client(), None, Some(fake)).expect("memory");
        let err = memory.put("user", "Ada").unwrap_err();
        assert!(err.to_string().contains("every backend"), "{err}");
        let err = memory.get("user").unwrap_err();
        assert!(err.to_string().contains("every backend"), "{err}");
    }
}
