//! `agent-sandbox` — pluggable execution sandbox for tool calls.
//!
//! Provides a provider abstraction so the same agent runtime can run under
//! **Docker**, **Kata**, or **Cube** sandboxes. The **Docker** provider talks to
//! the Docker **Engine API** through `bollard` — no `docker` CLI is required
//! inside the container; it connects to the daemon over the mounted host socket
//! (or whatever `DOCKER_HOST` points at). The **Kata** and **Cube** providers
//! remain thin CLI runners that shell out to the platform binary
//! (`docker --runtime=kata`, `cube`, …). The [`Sandbox`] trait is the stable
//! seam the rest of the platform depends on
//! (see `docs/adr/0003-sandbox-providers.md`).
//!
//! The deep codex integration (ADR-0005 §Decision) — running tool commands
//! through codex's real `SandboxManager` — lives in the `aria-agent-cloud`
//! runtime crate (`publish = false`) as `codex_sandbox::CodexSandbox`. Keeping
//! it out of this published crate avoids dragging codex's unpublished git
//! dependencies into the crates.io manifest (crates.io requires every
//! dependency to resolve from the registry). `SandboxProvider::Codex` remains a
//! valid selector so config stays forward-compatible; `from_provider` returns
//! [`SandboxError::NotConfigured`] for it and the cloud runtime injects the real
//! backend via `agent_core::Agent::with_sandbox`.

use async_trait::async_trait;
use bollard::container::{
    Config, CreateContainerOptions, RemoveContainerOptions, StartContainerOptions,
};
use bollard::exec::{CreateExecOptions, StartExecResults};
use bollard::Docker;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Specification of a command to execute inside a sandbox.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecSpec {
    /// Container/VM image to run (provider-specific default if `None`).
    pub image: Option<String>,
    /// The command and its arguments.
    pub command: Vec<String>,
    /// Working directory inside the sandbox.
    pub workdir: Option<String>,
    /// Environment variables.
    pub env: Vec<(String, String)>,
    /// Wall-clock timeout in milliseconds.
    pub timeout_ms: u64,
}

impl ExecSpec {
    pub fn command(command: Vec<String>) -> Self {
        Self {
            image: None,
            command,
            workdir: None,
            env: Vec::new(),
            timeout_ms: 60_000,
        }
    }
}

/// Result of an execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Opaque handle to a spawned sandbox session.
#[derive(Debug, Clone)]
pub struct SandboxHandle {
    pub id: String,
}

#[derive(Debug, Error)]
pub enum SandboxError {
    #[error("sandbox spawn failed: {0}")]
    Spawn(String),
    #[error("sandbox exec failed: {0}")]
    Exec(String),
    #[error("sandbox not configured: {0}")]
    NotConfigured(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Stable sandbox contract used by `agent-core` and `agent-cloud`.
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// Spawn a persistent sandbox session and return a handle.
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError>;
    /// Execute `cmd` inside a previously spawned session.
    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError>;
    /// Tear down a sandbox session.
    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError>;
}

/// Which sandbox backend to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxProvider {
    Docker,
    Kata,
    Cube,
    /// Deep codex integration: run tool commands through codex's `SandboxManager`
    /// (ADR-0005). This is the production backend for the agent runtime.
    Codex,
}

impl SandboxProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "docker" => Some(SandboxProvider::Docker),
            "kata" => Some(SandboxProvider::Kata),
            "cube" => Some(SandboxProvider::Cube),
            "codex" => Some(SandboxProvider::Codex),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxProvider::Docker => "docker",
            SandboxProvider::Kata => "kata",
            SandboxProvider::Cube => "cube",
            SandboxProvider::Codex => "codex",
        }
    }
}

/// Generic CLI-backed sandbox. `runner` is the binary (e.g. `docker`);
/// `run_args` are the args inserted before the image (e.g. `run --rm`, or
/// `run --runtime=kata --rm`).
struct CliSandbox {
    provider: SandboxProvider,
    runner: String,
    run_args: Vec<String>,
    default_image: String,
}

impl CliSandbox {
    fn new(
        provider: SandboxProvider,
        runner: &str,
        run_args: Vec<String>,
        default_image: &str,
    ) -> Self {
        Self {
            provider,
            runner: runner.to_string(),
            run_args,
            default_image: default_image.to_string(),
        }
    }

    fn image_of(&self, spec: &ExecSpec) -> String {
        spec.image
            .clone()
            .unwrap_or_else(|| self.default_image.clone())
    }
}

#[async_trait]
impl Sandbox for CliSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        let image = self.image_of(spec);
        // Create a long-lived session container; real commands run via `exec`.
        let mut cmd = tokio::process::Command::new(&self.runner);
        cmd.args(&self.run_args).arg(&image).args(["sleep", "3600"]);
        let out = cmd
            .output()
            .await
            .map_err(|e| SandboxError::Spawn(e.to_string()))?;
        if !out.status.success() {
            return Err(SandboxError::Spawn(
                String::from_utf8_lossy(&out.stderr).to_string(),
            ));
        }
        let id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Ok(SandboxHandle { id })
    }

    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        if cmd.is_empty() {
            return Err(SandboxError::Exec("empty command".into()));
        }
        let joined = shell_join(cmd);
        let mut command = tokio::process::Command::new(&self.runner);
        command
            .arg("exec")
            .arg(&handle.id)
            .args(["sh", "-c", &joined]);
        let out = command
            .output()
            .await
            .map_err(|e| SandboxError::Exec(e.to_string()))?;
        Ok(ExecOutput {
            exit_code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).to_string(),
            stderr: String::from_utf8_lossy(&out.stderr).to_string(),
        })
    }

    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        let mut command = tokio::process::Command::new(&self.runner);
        command.arg("rm").arg("-f").arg(&handle.id);
        let out = command
            .output()
            .await
            .map_err(|e| SandboxError::Exec(e.to_string()))?;
        if !out.status.success() {
            tracing::warn!(
                provider = self.provider.as_str(),
                stderr = %String::from_utf8_lossy(&out.stderr),
                "sandbox destroy reported a non-zero status"
            );
        }
        Ok(())
    }
}

/// Docker sandbox — the **default** provider.
///
/// Talks to the Docker **Engine API** through `bollard` — no `docker` CLI is
/// required inside the container. This matches the playground's deployment model:
/// the Compose `cloud` service mounts the host `/var/run/docker.sock` (DooD) and
/// the API is reached directly over that socket (or whatever `DOCKER_HOST`
/// points at). `DOCKER_HOST` is honored (e.g. `unix:///var/run/docker.sock` or a
/// remote `tcp://…`); when unset it falls back to the local unix socket.
pub struct DockerSandbox {
    docker: Docker,
}

impl DockerSandbox {
    pub fn new() -> Self {
        // `connect_with_local_defaults` honors `DOCKER_HOST` and falls back to the
        // unix socket when it is unset. If `DOCKER_HOST` is malformed we fall back
        // to the socket default so construction never fails.
        let docker = Docker::connect_with_local_defaults().unwrap_or_else(|_| {
            Docker::connect_with_socket_defaults().expect("failed to connect to the Docker socket")
        });
        Self { docker }
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| "alpine:latest".to_string());
        let name = format!("aria-sandbox-{}", uuid::Uuid::new_v4());
        self.docker
            .create_container(
                Some(CreateContainerOptions {
                    name: &name,
                    platform: None,
                }),
                Config {
                    image: Some(image),
                    cmd: Some(vec!["sleep".to_string(), "3600".to_string()]),
                    tty: Some(false),
                    env: Some(env_to_docker(&spec.env)),
                    working_dir: spec.workdir.clone(),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SandboxError::Spawn(e.to_string()))?;
        self.docker
            .start_container(&name, None::<StartContainerOptions<String>>)
            .await
            .map_err(|e| SandboxError::Spawn(e.to_string()))?;
        Ok(SandboxHandle { id: name })
    }

    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        if cmd.is_empty() {
            return Err(SandboxError::Exec("empty command".into()));
        }
        let joined = shell_join(cmd);
        let exec = self
            .docker
            .create_exec(
                &handle.id,
                CreateExecOptions {
                    cmd: Some(vec!["sh".to_string(), "-c".to_string(), joined]),
                    attach_stdout: Some(true),
                    attach_stderr: Some(true),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SandboxError::Exec(e.to_string()))?;

        let id = exec.id.clone();
        match self
            .docker
            .start_exec(&id, None)
            .await
            .map_err(|e| SandboxError::Exec(e.to_string()))?
        {
            StartExecResults::Attached { mut output, .. } => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                use futures::StreamExt;
                while let Some(frame) = output.next().await {
                    match frame.map_err(|e| SandboxError::Exec(e.to_string()))? {
                        bollard::container::LogOutput::StdOut { message } => {
                            stdout.push_str(&String::from_utf8_lossy(&message));
                        }
                        bollard::container::LogOutput::StdErr { message } => {
                            stderr.push_str(&String::from_utf8_lossy(&message));
                        }
                        _ => {}
                    }
                }
                let exit_code = self
                    .docker
                    .inspect_exec(&id)
                    .await
                    .map(|r| r.exit_code.unwrap_or(-1) as i32)
                    .unwrap_or(-1);
                Ok(ExecOutput {
                    exit_code,
                    stdout,
                    stderr,
                })
            }
            StartExecResults::Detached => Err(SandboxError::Exec(
                "docker exec returned a detached stream".into(),
            )),
        }
    }

    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        self.docker
            .remove_container(
                &handle.id,
                Some(RemoveContainerOptions {
                    force: true,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|e| SandboxError::Exec(e.to_string()))?;
        Ok(())
    }
}

/// Convert `(KEY, VALUE)` pairs into Docker's `KEY=VALUE` env strings.
fn env_to_docker(env: &[(String, String)]) -> Vec<String> {
    env.iter().map(|(k, v)| format!("{k}={v}")).collect()
}

/// Kata sandbox — Docker with the `kata` runtime.
pub struct KataSandbox {
    inner: CliSandbox,
}

impl KataSandbox {
    pub fn new() -> Self {
        Self {
            inner: CliSandbox::new(
                SandboxProvider::Kata,
                "docker",
                vec![
                    "run".into(),
                    "--runtime".into(),
                    "kata".into(),
                    "--rm".into(),
                    "-d".into(),
                ],
                "alpine:latest",
            ),
        }
    }
}

#[async_trait]
impl Sandbox for KataSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        self.inner.spawn(spec).await
    }
    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        self.inner.exec(handle, cmd).await
    }
    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        self.inner.destroy(handle).await
    }
}

/// Cube sandbox — shells out to the `cube` CLI. The exact flags depend on the
/// deployed Cube runtime; adjust `run_args` per environment.
pub struct CubeSandbox {
    inner: CliSandbox,
}

impl CubeSandbox {
    pub fn new() -> Self {
        Self {
            inner: CliSandbox::new(
                SandboxProvider::Cube,
                "cube",
                // Plausible `cube` invocation; align with your deployment.
                vec!["sandbox".into(), "run".into(), "--rm".into()],
                "cube-image:latest",
            ),
        }
    }
}

#[async_trait]
impl Sandbox for CubeSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        self.inner.spawn(spec).await
    }
    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        self.inner.exec(handle, cmd).await
    }
    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        self.inner.destroy(handle).await
    }
}

impl Default for DockerSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for KataSandbox {
    fn default() -> Self {
        Self::new()
    }
}

impl Default for CubeSandbox {
    fn default() -> Self {
        Self::new()
    }
}

/// The platform default sandbox: Docker.
pub fn default_sandbox() -> Box<dyn Sandbox> {
    Box::new(DockerSandbox::new())
}

/// Build a sandbox from a provider selector (config-driven).
///
/// Returns [`SandboxError::NotConfigured`] for [`SandboxProvider::Codex`]: the
/// codex backend (`codex_sandbox::CodexSandbox`) is provided by the
/// `aria-agent-cloud` runtime (publish = false) so the published SDK stays free
/// of codex's unpublished git dependencies. The cloud runtime injects it via
/// `agent_core::Agent::with_sandbox`.
pub fn from_provider(provider: SandboxProvider) -> Result<Box<dyn Sandbox>, SandboxError> {
    match provider {
        SandboxProvider::Docker => Ok(Box::new(DockerSandbox::new())),
        SandboxProvider::Kata => Ok(Box::new(KataSandbox::new())),
        SandboxProvider::Cube => Ok(Box::new(CubeSandbox::new())),
        SandboxProvider::Codex => Err(SandboxError::NotConfigured(
            "codex sandbox backend is provided by the aria-agent-cloud runtime; \
             construct it there and inject via Agent::with_sandbox"
                .into(),
        )),
    }
}

fn shell_join(cmd: &[String]) -> String {
    cmd.iter()
        .map(|a| {
            if a.contains(char::is_whitespace) || a.contains('"') || a.contains('\'') {
                format!("'{}'", a.replace('\'', "'\\''"))
            } else {
                a.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_docker() {
        let s = default_sandbox();
        // type-erased; just ensure construction does not panic
        let _ = s;
    }

    #[test]
    fn provider_parsing() {
        assert_eq!(
            SandboxProvider::parse("docker"),
            Some(SandboxProvider::Docker)
        );
        assert_eq!(SandboxProvider::parse("KATA"), Some(SandboxProvider::Kata));
        assert_eq!(SandboxProvider::parse("cube"), Some(SandboxProvider::Cube));
        assert_eq!(SandboxProvider::parse("podman"), None);
        let _ = uuid::Uuid::new_v4();
    }

    #[test]
    fn shell_join_quotes() {
        assert_eq!(
            shell_join(&["echo".into(), "hello world".into()]),
            "echo 'hello world'"
        );
    }

    #[test]
    fn exec_spec_command_defaults() {
        let s = ExecSpec::command(vec!["echo".into(), "hi".into()]);
        assert_eq!(s.command, vec!["echo", "hi"]);
        assert_eq!(s.timeout_ms, 60_000);
        assert!(s.image.is_none());
        assert!(s.workdir.is_none());
        assert!(s.env.is_empty());
    }

    #[test]
    fn shell_join_empty_and_escapes() {
        assert_eq!(shell_join(&[]), "");
        // A token containing a double-quote is single-quoted (no inner escaping needed).
        assert_eq!(shell_join(&["a\"b".into()]), "'a\"b'");
        // A token containing a single-quote is escaped as '\''.
        assert_eq!(shell_join(&["it's".into()]), "'it'\\''s'");
    }

    #[test]
    fn provider_as_str_roundtrip() {
        for p in [
            SandboxProvider::Docker,
            SandboxProvider::Kata,
            SandboxProvider::Cube,
            SandboxProvider::Codex,
        ] {
            let s = p.as_str();
            assert_eq!(SandboxProvider::parse(s), Some(p));
        }
    }

    #[test]
    fn provider_parse_is_case_insensitive_and_rejects_unknown() {
        assert_eq!(
            SandboxProvider::parse("DOCKER"),
            Some(SandboxProvider::Docker)
        );
        assert_eq!(SandboxProvider::parse("Kata"), Some(SandboxProvider::Kata));
        assert_eq!(
            SandboxProvider::parse("codex"),
            Some(SandboxProvider::Codex)
        );
        assert_eq!(SandboxProvider::parse("podman"), None);
        assert_eq!(SandboxProvider::parse(""), None);
    }

    #[test]
    fn from_provider_builds_all_published_variants() {
        for p in [
            SandboxProvider::Docker,
            SandboxProvider::Kata,
            SandboxProvider::Cube,
        ] {
            let _ = from_provider(p).expect("published provider must build");
        }
        // `codex` is intentionally not constructible here (backend lives in the
        // cloud runtime); the selector is still parsed/serialized for config compat.
        assert!(matches!(
            from_provider(SandboxProvider::Codex),
            Err(SandboxError::NotConfigured(_))
        ));
        let _ = CubeSandbox::new();
        let _ = KataSandbox::new();
    }
}
