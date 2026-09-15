//! `agent-sandbox` — pluggable execution sandbox for tool calls.
//!
//! Reuses the shape of codex's `sandboxing` / `linux-sandbox` (`ExecSpec`,
//! `ExecOutput`) but adds a provider abstraction so the same agent runtime can
//! run under **Docker**, **Kata**, **Cube**, or **Codex** sandboxes.
//!
//! Most providers are thin CLI runners that shell out to the platform binary
//! (`docker`, `docker --runtime=kata`, `cube`, …). This keeps the crate
//! self-contained and runnable wherever the corresponding CLI is installed,
//! while the [`Sandbox`] trait is the stable seam the rest of the platform
//! depends on (see `docs/adr/0003-sandbox-providers.md`).
//!
//! [`CodexSandbox`] is the deep codex integration (ADR-0005 §Decision): it
//! delegates tool execution to codex's real `SandboxManager` (`transform` +
//! `spawn_process`) instead of a bespoke in-process executor. It is the
//! backend the agent runtime uses in production, while tests keep using the
//! local [`Sandbox`] implementation.

use async_trait::async_trait;
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
pub struct DockerSandbox {
    inner: CliSandbox,
}

impl DockerSandbox {
    pub fn new() -> Self {
        Self {
            inner: CliSandbox::new(
                SandboxProvider::Docker,
                "docker",
                vec!["run".into(), "--rm".into(), "-d".into()],
                "alpine:latest",
            ),
        }
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
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

/// Codex-backed sandbox: the deep codex integration (ADR-0005 §Decision).
///
/// Rather than shelling out to a platform CLI, tool execution is delegated to
/// codex's real [`codex_sandboxing::SandboxManager`]: each command is
/// `transform`ed into a validated host-native launch request and then spawned
/// via [`codex_sandboxing::spawn_process`]. With [`codex_sandboxing::SandboxType::None`]
/// the command runs unsandboxed (no seccomp/landlock wrapper, no extra
/// executable required); stricter [`codex_sandboxing::SandboxType`] values can
/// be selected by adjusting [`run_via_codex`] if the environment ships the
/// codex-linux-sandbox binary.
///
/// The [`Sandbox`] trait is session-oriented (`spawn` → `exec` → `destroy`);
/// codex spawns a fresh process per command, so `spawn`/`destroy` are no-ops
/// and `exec` performs the full transform + spawn + capture.
pub struct CodexSandbox;

impl CodexSandbox {
    pub fn new() -> Self {
        Self
    }
}

impl Default for CodexSandbox {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Sandbox for CodexSandbox {
    async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        Ok(SandboxHandle { id: "codex".into() })
    }

    async fn exec(
        &self,
        _handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        if cmd.is_empty() {
            return Err(SandboxError::Exec("empty command".into()));
        }
        run_via_codex(cmd).await
    }

    async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// Run `cmd` through codex's `SandboxManager` and capture its output.
async fn run_via_codex(cmd: &[String]) -> Result<ExecOutput, SandboxError> {
    use codex_protocol::config_types::WindowsSandboxLevel;
    use codex_protocol::models::PermissionProfile;
    use codex_sandboxing::spawn_process;
    use codex_sandboxing::SandboxCommand;
    use codex_sandboxing::SandboxManager;
    use codex_sandboxing::SandboxTransformRequest;
    use codex_sandboxing::SandboxType;
    use codex_utils_absolute_path::AbsolutePathBuf;
    use codex_utils_path_uri::PathUri;
    use std::convert::TryFrom;
    use std::ffi::OsString;

    let cwd = std::env::current_dir().map_err(|e| SandboxError::Exec(e.to_string()))?;
    let abs = AbsolutePathBuf::try_from(cwd.as_path())
        .map_err(|_| SandboxError::Exec("current dir is not absolute".into()))?;
    let uri = PathUri::from_abs_path(&abs);

    let (program, args) = cmd
        .split_first()
        .expect("non-empty command checked by caller");
    let command = SandboxCommand {
        program: OsString::from(program.as_str()),
        args: args.to_vec(),
        cwd: uri.clone(),
        env: std::collections::HashMap::new(),
        managed_network: None,
        additional_permissions: None,
    };

    let request = SandboxManager::new()
        .transform(SandboxTransformRequest {
            command,
            permissions: &PermissionProfile::default(),
            sandbox: SandboxType::None,
            enforce_managed_network: false,
            environment_id: None,
            network: None,
            sandbox_policy_cwd: &uri,
            codex_linux_sandbox_exe: None,
            use_legacy_landlock: false,
            windows_sandbox_level: WindowsSandboxLevel::default(),
            windows_sandbox_private_desktop: false,
        })
        .map_err(|e| SandboxError::Exec(e.to_string()))?;

    let mut spawned = spawn_process(codex_sandboxing::SpawnRequest {
        command: &request.command,
        cwd: request
            .cwd
            .to_abs_path()
            .map_err(|e| SandboxError::Exec(e.to_string()))?
            .as_path(),
        env: &request.env,
        arg0: &request.arg0,
        sandbox: request.sandbox,
        windows_sandbox: None,
        tty: false,
        stdin_open: false,
        inherited_fds: &[],
    })
    .await
    .map_err(|e| SandboxError::Exec(e.to_string()))?;

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    while let Some(chunk) = spawned.stdout_rx.recv().await {
        stdout.extend_from_slice(&chunk);
    }
    while let Some(chunk) = spawned.stderr_rx.recv().await {
        stderr.extend_from_slice(&chunk);
    }
    let exit_code = spawned.exit_rx.await.unwrap_or(-1);
    Ok(ExecOutput {
        exit_code,
        stdout: String::from_utf8_lossy(&stdout).to_string(),
        stderr: String::from_utf8_lossy(&stderr).to_string(),
    })
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
pub fn from_provider(provider: SandboxProvider) -> Box<dyn Sandbox> {
    match provider {
        SandboxProvider::Docker => Box::new(DockerSandbox::new()),
        SandboxProvider::Kata => Box::new(KataSandbox::new()),
        SandboxProvider::Cube => Box::new(CubeSandbox::new()),
        SandboxProvider::Codex => Box::new(CodexSandbox::new()),
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
    fn from_provider_builds_all_variants() {
        for p in [
            SandboxProvider::Docker,
            SandboxProvider::Kata,
            SandboxProvider::Cube,
            SandboxProvider::Codex,
        ] {
            let _ = from_provider(p); // must construct without panicking
        }
        let _ = CubeSandbox::new();
        let _ = KataSandbox::new();
        let _ = CodexSandbox::new();
    }

    #[tokio::test]
    async fn codex_sandbox_exec_runs_command_through_codex_manager() {
        // Exercises the deep codex integration (ADR-0005): the command is run
        // via codex's `SandboxManager`, not a bespoke CLI seam.
        let sandbox = CodexSandbox::new();
        let handle = sandbox
            .spawn(&ExecSpec::command(vec!["echo".into(), "hi".into()]))
            .await
            .unwrap();
        let out = sandbox
            .exec(&handle, &["echo".into(), "codex-sandbox".into()])
            .await
            .expect("codex SandboxManager spawn must succeed");
        assert!(out.stdout.contains("codex-sandbox"));
        assert_eq!(out.exit_code, 0);
        sandbox.destroy(handle).await.unwrap();
    }

    #[tokio::test]
    async fn codex_sandbox_rejects_empty_command() {
        let sandbox = CodexSandbox::new();
        let handle = sandbox
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap();
        let res = sandbox.exec(&handle, &[]).await;
        assert!(matches!(res, Err(SandboxError::Exec(_))));
    }
}
