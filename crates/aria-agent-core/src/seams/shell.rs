//! Shell seam.
//!
//! Consumers hand over an **argv array** — never a shell string — and the
//! provider decides where it runs (local, sandbox, remote). dsh makes the same
//! point: swapping `bash-local` for a sandboxed or PowerShell executor requires
//! no change in `tool-bash` (ADR-0016).

use super::SeamResult;
use agent_sandbox::{ExecSpec, Sandbox};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Combined result of one command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShellOutput {
    pub exit_code: i64,
    #[serde(default)]
    pub stdout: String,
    #[serde(default)]
    pub stderr: String,
}

impl ShellOutput {
    pub fn success(&self) -> bool {
        self.exit_code == 0
    }

    /// `exit=0` + streams, the shape fed back to the model.
    pub fn render(&self) -> String {
        format!(
            "exit={} stdout={} stderr={}",
            self.exit_code, self.stdout, self.stderr
        )
    }
}

#[async_trait]
pub trait ShellSeam: Send + Sync {
    /// Run `argv`. Empty argv is an error, not a no-op.
    async fn exec(&self, argv: &[String], cwd: Option<&str>) -> SeamResult<ShellOutput>;
}

/// Provider that runs commands inside the `sandbox` seam backend.
pub struct SandboxShell {
    sandbox: Arc<dyn Sandbox>,
    cwd: Option<String>,
}

impl SandboxShell {
    pub fn new(sandbox: Arc<dyn Sandbox>) -> Self {
        Self { sandbox, cwd: None }
    }

    /// Default working directory for every command (typically the policy root).
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }
}

#[async_trait]
impl ShellSeam for SandboxShell {
    async fn exec(&self, argv: &[String], cwd: Option<&str>) -> SeamResult<ShellOutput> {
        if argv.is_empty() {
            return Err(super::SeamError::Invalid("empty command".into()));
        }
        let mut spec = ExecSpec::command(argv.to_vec());
        spec.workdir = cwd.map(|c| c.to_string()).or_else(|| self.cwd.clone());
        let handle = self
            .sandbox
            .spawn(&spec)
            .await
            .map_err(|e| super::SeamError::Unavailable(format!("sandbox spawn: {e}")))?;
        let out = self.sandbox.exec(&handle, argv).await;
        let _ = self.sandbox.destroy(handle).await;
        let out = out.map_err(|e| super::SeamError::Io(format!("sandbox exec: {e}")))?;
        Ok(ShellOutput {
            exit_code: out.exit_code as i64,
            stdout: out.stdout,
            stderr: out.stderr,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_sandbox::{ExecOutput, ExecSpec, SandboxError, SandboxHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Recording {
        calls: AtomicUsize,
        last: std::sync::Mutex<Option<Vec<String>>>,
        fail_exec: bool,
    }

    #[async_trait]
    impl Sandbox for Recording {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Ok(SandboxHandle { id: "rec".into() })
        }

        async fn exec(
            &self,
            _handle: &SandboxHandle,
            cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.last.lock().unwrap() = Some(cmd.to_vec());
            if self.fail_exec {
                return Err(SandboxError::Exec("boom".into()));
            }
            Ok(ExecOutput {
                exit_code: 0,
                stdout: "ok".into(),
                stderr: String::new(),
            })
        }

        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn sandbox_shell_forwards_argv_and_renders() {
        let sb = Arc::new(Recording::default());
        let shell = SandboxShell::new(sb.clone());
        let out = shell
            .exec(&["echo".into(), "hi".into()], None)
            .await
            .unwrap();
        assert!(out.success());
        assert_eq!(out.stdout, "ok");
        assert_eq!(out.render(), "exit=0 stdout=ok stderr=");
        assert_eq!(sb.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            sb.last.lock().unwrap().clone().unwrap(),
            vec!["echo".to_string(), "hi".to_string()]
        );
    }

    #[tokio::test]
    async fn empty_argv_is_rejected_without_touching_the_sandbox() {
        let sb = Arc::new(Recording::default());
        let shell = SandboxShell::new(sb.clone());
        let err = shell.exec(&[], None).await.unwrap_err();
        assert!(matches!(err, super::super::SeamError::Invalid(_)));
        assert_eq!(sb.calls.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn exec_failure_maps_to_io_error() {
        let sb = Arc::new(Recording {
            fail_exec: true,
            ..Default::default()
        });
        let shell = SandboxShell::new(sb);
        let err = shell.exec(&["true".into()], None).await.unwrap_err();
        assert!(matches!(err, super::super::SeamError::Io(_)), "{err:?}");
    }

    #[tokio::test]
    async fn default_cwd_is_applied() {
        let sb = Arc::new(Recording::default());
        let shell = SandboxShell::new(sb.clone()).with_cwd("/tmp");
        shell.exec(&["ls".into()], None).await.unwrap();
        assert_eq!(sb.calls.load(Ordering::SeqCst), 1);
    }
}
