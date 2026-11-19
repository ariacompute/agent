//! Transport for the dsh JSON-RPC surface.
//!
//! dsh's `sdk` profile speaks **newline-delimited JSON-RPC 2.0 over stdio**
//! (`python/sdk/README.md`). The trait exists so tests can drive the client
//! without a child process (the coverage gate forbids live dependencies).
//!
//! Two hard limits protect the process boundary:
//!
//! * a single frame is capped ([`DEFAULT_MAX_FRAME_BYTES`]); a runtime that
//!   streams an oversized line is a protocol error, not a memory blow-up;
//! * stderr is always drained (a full pipe would block the child) but only
//!   sampled into `tracing` at debug level, truncated — never the model output
//!   or credentials.

use async_trait::async_trait;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};

use agent_core::CoreError;

/// Upper bound on one JSON-RPC line (8 MiB).
pub const DEFAULT_MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// Bytes of child stderr kept for diagnostics.
const STDERR_SAMPLE_BYTES: usize = 2 * 1024;

/// One bidirectional line channel to the runtime.
#[async_trait]
pub trait RpcTransport: Send + Sync {
    /// Write `line` plus a newline.
    async fn send_line(&self, line: &str) -> Result<(), CoreError>;
    /// Read one line. `Ok(None)` means the runtime closed the pipe.
    async fn recv_line(&self) -> Result<Option<String>, CoreError>;
    /// Terminate the runtime and release the pipes (idempotent).
    async fn close(&self) -> Result<(), CoreError>;
}

/// `dsh --profile sdk` over a real child process.
pub struct StdioTransport {
    child: tokio::sync::Mutex<Child>,
    stdin: tokio::sync::Mutex<ChildStdin>,
    stdout: tokio::sync::Mutex<BufReader<ChildStdout>>,
    max_frame: usize,
}

impl StdioTransport {
    /// Spawn `bin args...` and wire its stdio.
    ///
    /// `env` is an explicit allow-list (`DSH_HOME`, provider base URL/key,
    /// `PATH` the caller chose): the child never inherits the whole environment,
    /// so a deployment cannot leak unrelated secrets into the harness.
    pub fn spawn(
        bin: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: &Path,
        max_frame: usize,
    ) -> Result<Self, CoreError> {
        let mut cmd = Command::new(bin);
        cmd.args(args)
            .current_dir(cwd)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            cmd.env(k, v);
        }
        // Governance: the harness makes outbound calls too. Forward the proxy
        // variables explicitly (they are part of the allow-list, not an
        // inheritance leak) so an external harness cannot silently bypass the
        // egress gateway that governs the native loop (ADR-0020).
        for var in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "NO_PROXY"] {
            if let Ok(value) = std::env::var(var) {
                cmd.env(var, value);
            }
        }
        let mut child = cmd.spawn().map_err(|e| {
            CoreError::Config(format!(
                "cannot start DeepSeek Harness `{bin}`: {e} \
                 (install `deepseek-harness-runtime-bin` or set DSH_BIN, \
                 or set ARIA_AGENT_HARNESS=native)"
            ))
        })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Config("dsh stdin unavailable".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Config("dsh stdout unavailable".into()))?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                let mut sample = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    match stderr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => {
                            if sample.len() < STDERR_SAMPLE_BYTES {
                                let room = STDERR_SAMPLE_BYTES - sample.len();
                                sample.extend_from_slice(&buf[..n.min(room)]);
                            }
                        }
                    }
                }
                if !sample.is_empty() {
                    tracing::debug!(
                        stderr = %String::from_utf8_lossy(&sample),
                        "dsh runtime stderr"
                    );
                }
            });
        }
        Ok(Self {
            child: tokio::sync::Mutex::new(child),
            stdin: tokio::sync::Mutex::new(stdin),
            stdout: tokio::sync::Mutex::new(BufReader::new(stdout)),
            max_frame,
        })
    }
}

#[async_trait]
impl RpcTransport for StdioTransport {
    async fn send_line(&self, line: &str) -> Result<(), CoreError> {
        let mut stdin = self.stdin.lock().await;
        let failure = |what: &str, e: std::io::Error| {
            CoreError::Model(format!("dsh transport {what} failed: {e}"))
        };
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| failure("write", e))?;
        stdin
            .write_all(b"\n")
            .await
            .map_err(|e| failure("write", e))?;
        stdin.flush().await.map_err(|e| failure("flush", e))
    }

    async fn close(&self) -> Result<(), CoreError> {
        let mut child = self.child.lock().await;
        match child.try_wait() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => child
                .start_kill()
                .map_err(|e| CoreError::Model(format!("dsh kill failed: {e}"))),
            Err(e) => Err(CoreError::Model(format!("dsh wait failed: {e}"))),
        }
    }

    async fn recv_line(&self) -> Result<Option<String>, CoreError> {
        let mut stdout = self.stdout.lock().await;
        let mut line = String::new();
        let read = stdout
            .read_line(&mut line)
            .await
            .map_err(|e| CoreError::Model(format!("dsh transport read failed: {e}")))?;
        if read == 0 {
            return Ok(None);
        }
        if line.len() > self.max_frame {
            return Err(CoreError::Model(format!(
                "dsh frame exceeds {} bytes (protocol error)",
                self.max_frame
            )));
        }
        Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()))
    }
}

/// Shared handle so the reader task and the client agree on the transport.
pub type Transport = Arc<dyn RpcTransport>;
