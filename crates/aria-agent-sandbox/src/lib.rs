//! `agent-sandbox` — pluggable execution sandbox for tool calls.
//!
//! Provides a provider abstraction so the same agent runtime can run under
//! **Docker**, **Kata**, or **Cube** sandboxes. The **Docker** provider talks to
//! the Docker **Engine API** through `bollard` — no `docker` CLI is required
//! inside the container; they connect to the daemon over the mounted host socket
//! (or whatever `DOCKER_HOST` points at). Kata is the same Engine API path but
//! runs containers under the `kata` runtime (`HostConfig.runtime`). The **Cube**
//! provider remains a thin CLI runner that shells out to the `cube` binary. The
//! [`Sandbox`] trait is the stable
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
use bollard::{Docker, API_DEFAULT_VERSION};
use serde::{Deserialize, Serialize};
use std::env;
use thiserror::Error;
use tokio::sync::OnceCell;

/// Resource limits applied to every sandbox container/VM.
///
/// Parsed once from `.env` (`SANDBOX_*`) at construction; any parse error falls
/// back to conservative defaults instead of panicking — the sandbox must build
/// on hosts where these vars are unset. The Docker/Kata backends push these
/// straight into a bollard `HostConfig`; the Cube backend best-effort maps them
/// to `cube` CLI flags.
#[derive(Debug, Clone, Copy)]
pub struct SandboxResourceLimits {
    /// Logical CPUs (e.g. `1.0`, `0.5`).
    pub cpus: f64,
    /// Memory cap in bytes (human-readable units are parsed by [`parse_size`]).
    pub memory_bytes: i64,
    /// Maximum number of processes (pids cgroup).
    pub pids_limit: i64,
}

impl SandboxResourceLimits {
    /// Read the limits from the process environment, falling back to
    /// `cpus=1.0`, `memory=512m`, `pids_limit=256` on any parse error.
    pub fn from_env() -> Self {
        Self {
            cpus: parse_cpu(env::var("SANDBOX_CPUS").ok(), 1.0),
            memory_bytes: parse_memory(env::var("SANDBOX_MEMORY").ok(), 512 * 1024 * 1024),
            pids_limit: parse_pids(env::var("SANDBOX_PIDS_LIMIT").ok(), 256),
        }
    }
}

/// Build a bollard `HostConfig` carrying the resource limits. `runtime` is only
/// set for Kata (`"kata"`); `None` means the default Docker runtime.
///
/// `memory_swap = -1` disables swap accounting so a memory cap does not trip the
/// cgroup "memory swap must be >= memory" constraint.
pub fn build_host_config(
    res: &SandboxResourceLimits,
    runtime: Option<&str>,
) -> bollard::service::HostConfig {
    bollard::service::HostConfig {
        nano_cpus: Some((res.cpus * 1e9).round() as i64),
        memory: Some(res.memory_bytes),
        memory_swap: Some(-1),
        pids_limit: Some(res.pids_limit),
        runtime: runtime.map(|s| s.to_string()),
        ..Default::default()
    }
}

fn parse_cpu(raw: Option<String>, default: f64) -> f64 {
    match raw {
        Some(s) => match s.trim().parse::<f64>() {
            Ok(v) if v > 0.0 => v,
            _ => {
                tracing::warn!(value = %s.trim(), default, "invalid SANDBOX_CPUS; using default");
                default
            }
        },
        None => default,
    }
}

fn parse_memory(raw: Option<String>, default: i64) -> i64 {
    match raw {
        Some(s) => match parse_size(&s) {
            Some(v) => v,
            None => {
                tracing::warn!(value = %s.trim(), default, "invalid SANDBOX_MEMORY; using default");
                default
            }
        },
        None => default,
    }
}

fn parse_pids(raw: Option<String>, default: i64) -> i64 {
    match raw {
        Some(s) => match s.trim().parse::<i64>() {
            Ok(v) if v > 0 => v,
            _ => {
                tracing::warn!(value = %s.trim(), default, "invalid SANDBOX_PIDS_LIMIT; using default");
                default
            }
        },
        None => default,
    }
}

/// Parse a human-readable size (`512m`, `1g`, `1024`) into bytes. No unit or `b`
/// means bytes; `k/m/g/t` are powers of 1024. Returns `None` on any error or a
/// non-positive value.
fn parse_size(s: &str) -> Option<i64> {
    let s = s.trim();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let value: f64 = num.trim().parse().ok()?;
    let mult: i64 = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" => 1024,
        "m" => 1024 * 1024,
        "g" => 1024 * 1024 * 1024,
        "t" => 1024 * 1024 * 1024 * 1024,
        _ => return None,
    };
    if value <= 0.0 {
        return None;
    }
    Some((value * mult as f64) as i64)
}

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
    /// Browser sandbox: the agent's tool calls are served by an embedded browser
    /// running in a dedicated container. The `BrowserSandbox` backend lives in the
    /// `aria-agent-browser` crate (publish = false concerns aside, it is selected
    /// by the cloud/FFI runtimes and injected via `Agent::with_handlers`), so
    /// `from_provider` returns `NotConfigured` here — mirroring `Codex`.
    Browser,
}

impl SandboxProvider {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "docker" => Some(SandboxProvider::Docker),
            "kata" => Some(SandboxProvider::Kata),
            "cube" => Some(SandboxProvider::Cube),
            "codex" => Some(SandboxProvider::Codex),
            "browser" => Some(SandboxProvider::Browser),
            _ => None,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            SandboxProvider::Docker => "docker",
            SandboxProvider::Kata => "kata",
            SandboxProvider::Cube => "cube",
            SandboxProvider::Codex => "codex",
            SandboxProvider::Browser => "browser",
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

    /// CLI args inserted before the image (introspection aid for tests).
    #[cfg(test)]
    fn run_args(&self) -> &[String] {
        &self.run_args
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

/// Connect timeout (seconds) used when probing Docker sockets.
const DOCKER_TIMEOUT_SECS: u64 = 120;

/// Well-known Docker daemon socket locations, probed in order.
///
/// `/var/run/docker.sock` is the classic Linux location, but it is **not**
/// universal: Docker Desktop on macOS exposes the daemon at
/// `~/.docker/run/docker.sock` (and only symlinks `/var/run/docker.sock` when
/// the "default socket" option is enabled), Colima uses
/// `~/.colima/default/docker.sock`, and rootless setups (Docker Desktop on
/// Linux, Podman) live under `XDG_RUNTIME_DIR`. `ARIA_DOCKER_SOCKET` and
/// `DOCKER_HOST` (`unix://…`) always win when set.
fn candidate_sockets() -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |p: String| {
        let p = p.trim().to_string();
        if !p.is_empty() && !out.contains(&p) {
            out.push(p);
        }
    };

    if let Ok(p) = env::var("ARIA_DOCKER_SOCKET") {
        push(p);
    }
    if let Ok(host) = env::var("DOCKER_HOST") {
        // Non-unix schemes (e.g. `tcp://`) contribute no socket path; `push`
        // drops the empty string.
        push(host.strip_prefix("unix://").unwrap_or_default().to_string());
    }

    #[cfg(unix)]
    {
        push("/var/run/docker.sock".into());
        if let Ok(home) = env::var("HOME") {
            // Docker Desktop (macOS) and Colima.
            push(format!("{home}/.docker/run/docker.sock"));
            push(format!("{home}/.colima/default/docker.sock"));
        }
        if let Ok(xdg) = env::var("XDG_RUNTIME_DIR") {
            // Rootless Docker Desktop (Linux) and rootless Podman.
            push(format!("{xdg}/.docker/run/docker.sock"));
            push(format!("{xdg}/podman/podman.sock"));
        }
        push("/run/podman/podman.sock".into());
    }

    out
}

/// Try each candidate socket in order and return the first connection that
/// resolves. This only *constructs* a client (the socket file must exist); no
/// daemon round-trip is performed, so a stale socket still fails later at
/// `spawn`/`exec` with a normal [`SandboxError`].
fn connect_first(sockets: &[String]) -> Result<Docker, SandboxError> {
    let mut last: Option<String> = None;
    for path in sockets {
        match Docker::connect_with_socket(path, DOCKER_TIMEOUT_SECS, API_DEFAULT_VERSION) {
            Ok(docker) => {
                tracing::debug!(socket = %path, "connected to the Docker daemon");
                return Ok(docker);
            }
            Err(e) => {
                tracing::debug!(socket = %path, error = %e, "docker socket unavailable");
                last = Some(e.to_string());
            }
        }
    }
    Err(SandboxError::NotConfigured(format!(
        "no reachable Docker daemon: set DOCKER_HOST (or ARIA_DOCKER_SOCKET) to your daemon socket; probed: [{}]{}",
        sockets.join(", "),
        last.map(|e| format!(" (last error: {e})")).unwrap_or_default(),
    )))
}

/// Docker sandbox — the **default** provider.
///
/// Talks to the Docker **Engine API** through `bollard` — no `docker` CLI is
/// required inside the container. This matches the playground's deployment model:
/// the Compose `cloud` service mounts the host `/var/run/docker.sock` (DooD) and
/// the API is reached directly over that socket (or whatever `DOCKER_HOST`
/// points at). `DOCKER_HOST` is honored (e.g. `unix:///var/run/docker.sock` or a
/// remote `tcp://…`); when unset the well-known socket locations are probed.
///
/// The daemon connection is **lazy**: constructing a sandbox never panics and
/// never fails, so the agent runtime (and the whole test suite) works on
/// machines without a reachable daemon — including macOS, where Docker Desktop
/// does not expose `/var/run/docker.sock` by default. The connection is
/// established on the first `spawn`/`exec`/`destroy`, which then returns
/// [`SandboxError::NotConfigured`] if no daemon could be reached.
pub struct DockerSandbox {
    docker: OnceCell<Docker>,
    /// Explicit socket override; when set, discovery is skipped entirely.
    sockets: Option<Vec<String>>,
    /// Resource limits applied to every spawned sandbox container.
    resources: SandboxResourceLimits,
    /// Optional container runtime (e.g. `"kata"`). `None` = default runtime.
    runtime: Option<String>,
}

impl DockerSandbox {
    pub fn new() -> Self {
        Self {
            docker: OnceCell::new(),
            sockets: None,
            resources: SandboxResourceLimits::from_env(),
            runtime: None,
        }
    }

    /// Return the pinned container runtime, if any (e.g. `"kata"`). `None`
    /// means the daemon's default runtime.
    pub fn runtime(&self) -> Option<&str> {
        self.runtime.as_deref()
    }

    /// Build a sandbox around an explicit daemon client. Used by tests (and by
    /// embedders that already own a `bollard::Docker`), so the Docker I/O paths
    /// can be exercised against a stub Engine API instead of a live daemon.
    pub fn with_client(docker: Docker) -> Self {
        Self {
            docker: OnceCell::from(docker),
            sockets: None,
            resources: SandboxResourceLimits::from_env(),
            runtime: None,
        }
    }

    /// Pin the sandbox to one socket path (no `DOCKER_HOST`/auto-discovery).
    pub fn with_socket(path: impl Into<String>) -> Self {
        Self {
            docker: OnceCell::new(),
            sockets: Some(vec![path.into()]),
            resources: SandboxResourceLimits::from_env(),
            runtime: None,
        }
    }

    /// Build a sandbox that runs containers under a specific Docker runtime
    /// (e.g. `"kata"`). Used by [`KataSandbox`]; reuses the same bollard client
    /// and the same [`SandboxResourceLimits`] as the default Docker provider.
    pub fn with_runtime(runtime: impl Into<String>) -> Self {
        Self {
            docker: OnceCell::new(),
            sockets: None,
            resources: SandboxResourceLimits::from_env(),
            runtime: Some(runtime.into()),
        }
    }

    /// Resolve (and memoize) the daemon client.
    pub async fn client(&self) -> Result<&Docker, SandboxError> {
        self.docker
            .get_or_try_init(|| async {
                match &self.sockets {
                    Some(sockets) => connect_first(sockets),
                    // `connect_with_local_defaults` honors `DOCKER_HOST` (including
                    // `tcp://…` and the Windows named pipe) and TLS settings.
                    None => match Docker::connect_with_local_defaults() {
                        Ok(docker) => Ok(docker),
                        Err(_) => connect_first(&candidate_sockets()),
                    },
                }
            })
            .await
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        let docker = self.client().await?;
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| "alpine:latest".to_string());
        let name = format!("aria-sandbox-{}", uuid::Uuid::new_v4());
        docker
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
                    host_config: Some(build_host_config(&self.resources, self.runtime.as_deref())),
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| SandboxError::Spawn(e.to_string()))?;
        docker
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
        let docker = self.client().await?;
        let exec = docker
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
        match docker
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
                let exit_code = docker
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
        let docker = self.client().await?;
        docker
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

/// Kata sandbox — Docker Engine API with the `kata` runtime.
///
/// Kata containers are started exactly like Docker containers but with
/// `HostConfig.runtime = "kata"`, so they reuse the same bollard client and the
/// same [`SandboxResourceLimits`] as the default Docker provider. No `docker`
/// CLI is involved.
pub struct KataSandbox {
    inner: DockerSandbox,
}

impl KataSandbox {
    pub fn new() -> Self {
        Self {
            inner: DockerSandbox::with_runtime("kata"),
        }
    }

    /// Runtime this Kata sandbox pins (`"kata"`).
    pub fn runtime(&self) -> Option<&str> {
        self.inner.runtime()
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
        let res = SandboxResourceLimits::from_env();
        // `cube` is a standalone microVM runtime with its own CLI; it does **not**
        // speak the Docker Engine API, so resource limits are best-effort here.
        // These flags mirror docker-style options and may need adjustment for
        // your deployed Cube runtime.
        let run_args = vec![
            "sandbox".into(),
            "run".into(),
            "--rm".into(),
            "--cpus".into(),
            res.cpus.to_string(),
            "--memory".into(),
            res.memory_bytes.to_string(),
        ];
        Self {
            inner: CliSandbox::new(SandboxProvider::Cube, "cube", run_args, "cube-image:latest"),
        }
    }

    /// Resource flags this Cube sandbox passes to the `cube` CLI. Introspection
    /// aid for tests; the Cube runtime may require different flags per
    /// deployment, so this is not part of the public contract.
    #[cfg(test)]
    pub(crate) fn run_args(&self) -> &[String] {
        self.inner.run_args()
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
        // The browser backend lives in the `aria-agent-browser` crate; the cloud/FFI
        // runtimes construct `BrowserSandbox` and inject it as a `ToolHandler`
        // (via `Agent::with_handlers`) alongside the regular shell sandbox.
        SandboxProvider::Browser => Err(SandboxError::NotConfigured(
            "browser sandbox backend is provided by the aria-agent-browser crate; \
             construct it and inject via Agent::with_handlers"
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
    fn docker_sandbox_construction_never_panics() {
        // Regression: on macOS the daemon socket is not `/var/run/docker.sock`,
        // and construction used to `expect()` on that path.
        let _ = DockerSandbox::new();
        let _ = DockerSandbox::with_socket("/definitely/not/a/docker.sock");
        let _ = default_sandbox();
    }

    #[cfg(unix)]
    #[test]
    fn candidate_sockets_include_desktop_and_colima() {
        let cands = candidate_sockets();
        assert!(cands.contains(&"/var/run/docker.sock".to_string()));
        // Docker Desktop (macOS) and Colima keep the daemon under `$HOME`.
        let home = env::var("HOME").ok();
        for home in home.iter() {
            assert!(cands.contains(&format!("{home}/.docker/run/docker.sock")));
            assert!(cands.contains(&format!("{home}/.colima/default/docker.sock")));
        }
        // No duplicates.
        let mut seen = std::collections::HashSet::new();
        for c in &cands {
            assert!(seen.insert(c.clone()), "duplicate candidate: {c}");
        }
    }

    #[tokio::test]
    async fn resolve_docker_returns_error_instead_of_panicking() {
        // Whether or not a daemon exists locally, resolution must not panic;
        // when it fails it must be a `NotConfigured` naming the env override.
        match DockerSandbox::new().client().await {
            Ok(_) => {}
            Err(SandboxError::NotConfigured(msg)) => assert!(msg.contains("DOCKER_HOST")),
            Err(other) => panic!("unexpected error: {other}"),
        }
    }

    #[tokio::test]
    async fn unreachable_daemon_surfaces_error_not_panic() {
        let sandbox = DockerSandbox::with_socket("/definitely/not/a/docker.sock");
        let spec = ExecSpec::command(vec!["true".into()]);
        let res = sandbox.spawn(&spec).await;
        assert!(res.is_err(), "spawn must fail when no daemon is reachable");
        let err = sandbox
            .exec(&SandboxHandle { id: "nope".into() }, &["true".into()])
            .await;
        assert!(err.is_err());
        let destroyed = sandbox.destroy(SandboxHandle { id: "nope".into() }).await;
        assert!(destroyed.is_err());
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

    #[test]
    fn resource_limit_parsers_default_and_reject_bad_input() {
        // cpus
        assert_eq!(parse_cpu(Some("1.5".into()), 1.0), 1.5);
        assert_eq!(parse_cpu(Some("0".into()), 1.0), 1.0);
        assert_eq!(parse_cpu(Some("nope".into()), 2.0), 2.0);
        assert_eq!(parse_cpu(None, 1.0), 1.0);
        // memory
        assert_eq!(parse_memory(Some("512m".into()), 0), 512 * 1024 * 1024);
        assert_eq!(parse_memory(Some("1g".into()), 0), 1024 * 1024 * 1024);
        assert_eq!(parse_memory(Some("1024".into()), 0), 1024);
        assert_eq!(parse_memory(Some("bad".into()), 999), 999);
        assert_eq!(parse_memory(None, 123), 123);
        // pids
        assert_eq!(parse_pids(Some("100".into()), 256), 100);
        assert_eq!(parse_pids(Some("-1".into()), 256), 256);
        assert_eq!(parse_pids(None, 256), 256);
    }

    #[test]
    fn kata_sandbox_wraps_docker_with_kata_runtime() {
        // Kata must build without a daemon and wrap a Docker-backed sandbox.
        let _ = KataSandbox::new();
    }

    #[test]
    fn kata_runtime_is_kata() {
        // Kata must pin the Docker runtime to "kata" and build without a daemon.
        let kata = KataSandbox::new();
        assert_eq!(kata.runtime(), Some("kata"));
        let _ = kata;
    }

    #[test]
    fn cube_and_kata_construction_never_panics() {
        let _ = KataSandbox::new();
        let _ = CubeSandbox::new();
    }

    #[test]
    fn cube_sandbox_maps_resource_limits_to_cli_flags() {
        // The Cube CLI must carry the parsed SANDBOX_* values into its run args.
        // Computed against `from_env()` so the expectation stays in sync with the
        // actual parsing (deterministic when the vars are unset).
        let cube = CubeSandbox::new();
        let res = SandboxResourceLimits::from_env();
        let expected = [
            "sandbox".to_string(),
            "run".to_string(),
            "--rm".to_string(),
            "--cpus".to_string(),
            res.cpus.to_string(),
            "--memory".to_string(),
            res.memory_bytes.to_string(),
        ];
        assert_eq!(cube.run_args(), &expected[..]);
    }

    #[tokio::test]
    async fn kata_spawn_surfaces_error_without_daemon() {
        // Kata shares the bollard path with Docker: spawn/exec/destroy must
        // surface an error (not panic) when no daemon is reachable.
        let kata = KataSandbox::new();
        assert_eq!(kata.runtime(), Some("kata"));
        let spec = ExecSpec::command(vec!["true".into()]);
        let res = kata.spawn(&spec).await;
        assert!(
            res.is_err(),
            "kata spawn must fail when no daemon is reachable"
        );
    }

    #[tokio::test]
    async fn cube_missing_binary_surfaces_error_not_panic() {
        // Model the `cube` CLI being absent: route the same CliSandbox path that
        // `CubeSandbox` uses through a non-existent runner. spawn/exec must fail
        // with an error (not panic); destroy must never panic.
        let sandbox = CliSandbox::new(
            SandboxProvider::Cube,
            "definitely-no-such-binary-xyz",
            vec!["sandbox".into(), "run".into(), "--rm".into()],
            "cube-image:latest",
        );
        let spec = ExecSpec::command(vec!["true".into()]);
        let res = sandbox.spawn(&spec).await;
        assert!(
            res.is_err(),
            "spawn must error when the cube binary is missing"
        );
        let err = sandbox
            .exec(&SandboxHandle { id: "nope".into() }, &["true".into()])
            .await;
        assert!(
            err.is_err(),
            "exec must error when the cube binary is missing"
        );
        // destroy logs-and-continues on failure; it must not panic.
        let _ = sandbox.destroy(SandboxHandle { id: "nope".into() }).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cube_invalid_invocation_surfaces_error_not_panic() {
        // Model the `cube` CLI being present but rejecting our flags (or failing
        // for any reason): `false` is a real binary that always exits non-zero,
        // exercising the non-success -> Spawn error path through the same
        // CliSandbox that `CubeSandbox` uses. spawn/exec must surface an error,
        // never panic.
        let sandbox = CliSandbox::new(
            SandboxProvider::Cube,
            "false",
            vec!["sandbox".into(), "run".into(), "--rm".into()],
            "cube-image:latest",
        );
        let spec = ExecSpec::command(vec!["true".into()]);
        let res = sandbox.spawn(&spec).await;
        assert!(
            res.is_err(),
            "spawn must error when the cube invocation fails"
        );
        // exec spawns the present binary fine (no panic); the failed invocation
        // is reflected in the exit code, not as a spawn error. Only `spawn`
        // guards on success.
        let out = sandbox
            .exec(&SandboxHandle { id: "nope".into() }, &["true".into()])
            .await;
        assert!(
            out.is_ok(),
            "exec must not panic when the cube binary is present"
        );
        assert_eq!(out.unwrap().exit_code, 1);
        // destroy logs-and-continues on failure; it must not panic.
        let _ = sandbox.destroy(SandboxHandle { id: "nope".into() }).await;
    }

    // --- Stub Docker Engine API (raw TCP, so `exec` upgrade works) ---

    #[cfg(unix)]
    #[test]
    fn parser_accepts_terabytes_and_rejects_zero_or_bad_units() {
        assert_eq!(parse_memory(Some("1t".into()), 0), 1024i64.pow(4));
        assert_eq!(parse_memory(Some(" 1.5G ".into()), 0), 1536 * 1024 * 1024);
        // Unknown unit and non-positive values fall back to the default.
        assert_eq!(parse_memory(Some("1x".into()), 7), 7);
        assert_eq!(parse_memory(Some("0m".into()), 7), 7);
    }

    #[test]
    fn every_provider_roundtrips_including_browser() {
        for p in [
            SandboxProvider::Docker,
            SandboxProvider::Kata,
            SandboxProvider::Cube,
            SandboxProvider::Codex,
            SandboxProvider::Browser,
        ] {
            assert_eq!(SandboxProvider::parse(p.as_str()), Some(p));
        }
        // Browser and codex are out-of-crate backends: selectable, not buildable.
        assert!(matches!(
            from_provider(SandboxProvider::Browser),
            Err(SandboxError::NotConfigured(msg)) if msg.contains("browser")
        ));
    }

    #[test]
    fn default_impls_build_every_provider() {
        let _ = DockerSandbox::default();
        let _ = KataSandbox::default();
        let _ = CubeSandbox::default();
    }

    #[tokio::test]
    async fn kata_and_cube_delegate_to_their_inner_sandboxes() {
        // Without a daemon every delegate call fails; the point is that the
        // forwarding impls are the ones being exercised (not a panic).
        let kata = KataSandbox::default();
        let spec = ExecSpec::command(vec!["true".into()]);
        assert!(kata.spawn(&spec).await.is_err());
        assert!(kata
            .exec(&SandboxHandle { id: "x".into() }, &["true".into()])
            .await
            .is_err());
        assert!(kata
            .destroy(SandboxHandle { id: "x".into() })
            .await
            .is_err());

        let cube = CubeSandbox::default();
        assert!(cube.spawn(&spec).await.is_err());
        assert!(cube
            .exec(&SandboxHandle { id: "x".into() }, &["true".into()])
            .await
            .is_err());
        // destroy logs-and-continues.
        let _ = cube.destroy(SandboxHandle { id: "x".into() }).await;
    }

    static SOCKET_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[cfg(unix)]
    #[test]
    fn candidate_sockets_honour_the_environment_overrides() {
        let _lock = SOCKET_ENV_LOCK.lock().unwrap();
        // Pre-seed one variable so both restore arms are exercised: a value to
        // put back, and a variable that must end up removed.
        env::set_var("ARIA_DOCKER_SOCKET", "/tmp/aria-before.sock");
        env::remove_var("DOCKER_HOST");
        let saved_socket = env::var("ARIA_DOCKER_SOCKET").ok();
        let saved_host = env::var("DOCKER_HOST").ok();
        env::set_var("ARIA_DOCKER_SOCKET", "/tmp/aria-explicit.sock");
        env::set_var("DOCKER_HOST", "unix:///tmp/aria-docker-host.sock");
        let cands = candidate_sockets();
        env::set_var("DOCKER_HOST", "tcp://127.0.0.1:2375");
        let tcp_cands = candidate_sockets();
        for (key, value) in [
            ("ARIA_DOCKER_SOCKET", saved_socket),
            ("DOCKER_HOST", saved_host),
        ] {
            match value {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
        assert!(cands.contains(&"/tmp/aria-explicit.sock".to_string()));
        assert!(cands.contains(&"/tmp/aria-docker-host.sock".to_string()));
        // A `tcp://` DOCKER_HOST contributes no unix socket.
        assert!(!tcp_cands.contains(&"/tmp/aria-docker-host.sock".to_string()));
    }

    #[tokio::test]
    async fn client_accepts_an_existing_socket_file() {
        // `connect_with_socket` only needs the socket *file* to exist.
        let path = std::env::temp_dir().join(format!("aria-sock-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"").unwrap();
        let sandbox = DockerSandbox::with_socket(path.to_string_lossy().to_string());
        assert!(sandbox.client().await.is_ok());
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    // The guard is intentionally held across the await: it serializes the
    // `DOCKER_HOST` mutation for the whole resolution, and nothing inside the
    // awaited future tries to take it again.
    #[allow(clippy::await_holding_lock)]
    async fn client_falls_back_to_probing_when_local_defaults_fail() {
        let _lock = SOCKET_ENV_LOCK.lock().unwrap();
        let saved = env::var("DOCKER_HOST").ok();
        // Pointing at a socket that does not exist makes the local defaults fail.
        env::set_var("DOCKER_HOST", "unix:///tmp/aria-no-such-docker.sock");
        let sandbox = DockerSandbox::new();
        let result = sandbox.client().await.map(|_| ());
        match saved {
            Some(v) => env::set_var("DOCKER_HOST", v),
            None => env::remove_var("DOCKER_HOST"),
        }
        // The probe may still find a real daemon socket (e.g. a mounted
        // `/var/run/docker.sock`); what matters is that the fallback runs and
        // never panics.
        assert!(
            matches!(result, Ok(()) | Err(SandboxError::NotConfigured(_))),
            "unexpected fallback outcome: {result:?}"
        );
    }

    /// One request the stub daemon saw, in `METHOD path` form.
    fn request_line(head: &str) -> String {
        head.lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .take(2)
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Route {
        CreateContainer,
        StartContainer,
        CreateExec,
        StartExec,
        InspectExec,
        RemoveContainer,
        Other,
    }

    fn route_of(method: &str, path: &str) -> Route {
        let path = path.split('?').next().unwrap_or(path);
        if method == "DELETE" && path.contains("/containers/") {
            return Route::RemoveContainer;
        }
        if path.ends_with("/containers/create") {
            return Route::CreateContainer;
        }
        if path.ends_with("/exec") {
            return Route::CreateExec;
        }
        if path.contains("/exec/") && path.ends_with("/start") {
            return Route::StartExec;
        }
        if path.contains("/exec/") && path.ends_with("/json") {
            return Route::InspectExec;
        }
        if path.contains("/containers/") && path.ends_with("/start") {
            return Route::StartContainer;
        }
        Route::Other
    }

    /// A hand-rolled Docker Engine API stub.
    ///
    /// `bollard` starts `exec` with a raw-stream **upgrade**, which JSON mock
    /// servers cannot emulate, so this speaks HTTP over a plain TCP socket and
    /// writes multiplexed frames after the `101` response.
    struct EngineStub {
        endpoint: String,
        requests: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl EngineStub {
        fn start() -> Self {
            Self::start_with(Route::Other, false)
        }

        /// Serve normally except for `fail`, which answers `500`.
        fn start_failing(fail: Route) -> Self {
            Self::start_with(fail, false)
        }

        /// Answer `exec` with an upgraded stream that is cut off mid-frame.
        fn start_truncating() -> Self {
            Self::start_with(Route::Other, true)
        }

        fn start_with(fail: Route, truncate: bool) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let requests = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let seen = std::sync::Arc::clone(&requests);
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    serve(&mut stream, &seen, fail, truncate);
                }
            });
            Self { endpoint, requests }
        }

        fn client(&self) -> Docker {
            Docker::connect_with_http(&self.endpoint, 5, API_DEFAULT_VERSION).unwrap()
        }

        fn sandbox(&self) -> DockerSandbox {
            DockerSandbox::with_client(self.client())
        }

        fn requests(&self) -> Vec<String> {
            self.requests.lock().unwrap().clone()
        }
    }

    fn serve(
        stream: &mut std::net::TcpStream,
        seen: &std::sync::Mutex<Vec<String>>,
        fail: Route,
        truncate: bool,
    ) {
        use std::io::{Read, Write};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 128];
        let head_end = loop {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let line = request_line(&head);
        seen.lock().unwrap().push(line.clone());
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        let route = route_of(&method, &path);

        // Drain any declared body so the client is not left writing into a
        // closed socket.
        let declared = head
            .lines()
            .find_map(|l| {
                let l = l.to_ascii_lowercase();
                l.strip_prefix("content-length:")
                    .map(|v| v.trim().to_string())
            })
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(0);
        while buf.len() < head_end + declared {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }

        // `Route::Other` doubles as "never fail", so the 404 branch stays reachable.
        if route == fail && fail != Route::Other {
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
            );
            let _ = stream.flush();
            return;
        }

        match route {
            Route::CreateContainer => {
                json(stream, "201 Created", r#"{"Id":"cid-1","Warnings":[]}"#)
            }
            Route::StartContainer | Route::RemoveContainer => {
                let _ = stream.write_all(
                    b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
            Route::CreateExec => json(stream, "201 Created", r#"{"Id":"exec-1"}"#),
            Route::InspectExec => json(stream, "200 OK", r#"{"ID":"exec-1","ExitCode":0}"#),
            Route::StartExec => {
                let _ = stream.write_all(
                    b"HTTP/1.1 101 UPGRADED\r\nContent-Type: application/vnd.docker.raw-stream\r\nConnection: Upgrade\r\nUpgrade: tcp\r\n\r\n",
                );
                let _ = stream.flush();
                if truncate {
                    // Half a frame header, then EOF: the client's decoder errors.
                    use std::io::Write;
                    let _ = stream.write_all(&[1, 0]);
                    let _ = stream.flush();
                    let _ = stream.shutdown(std::net::Shutdown::Both);
                    return;
                }
                frame(stream, 1, b"hello\n");
                frame(stream, 2, b"oops\n");
                // stdin frames are ignored by the sandbox.
                frame(stream, 0, b"ignored\n");
                let _ = stream.flush();
                std::thread::sleep(std::time::Duration::from_millis(100));
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
            Route::Other => {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                let _ = stream.flush();
            }
        }
    }

    fn json(stream: &mut std::net::TcpStream, status: &str, body: &str) {
        use std::io::Write;
        let out = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(out.as_bytes());
        let _ = stream.flush();
    }

    fn frame(stream: &mut std::net::TcpStream, kind: u8, payload: &[u8]) {
        use std::io::Write;
        let mut buf = Vec::with_capacity(8 + payload.len());
        buf.push(kind);
        buf.extend_from_slice(&[0, 0, 0]);
        buf.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        buf.extend_from_slice(payload);
        let _ = stream.write_all(&buf);
    }

    fn stub_spec() -> ExecSpec {
        let mut spec = ExecSpec::command(vec!["true".into()]);
        spec.env = vec![("ARIA".into(), "1".into())];
        spec.workdir = Some("/work".into());
        spec
    }

    #[tokio::test]
    async fn docker_spawn_exec_and_destroy_against_a_stub_engine() {
        let stub = EngineStub::start();
        let sandbox = stub.sandbox();

        let handle = sandbox.spawn(&stub_spec()).await.unwrap();
        assert!(handle.id.starts_with("aria-sandbox-"), "{}", handle.id);

        let out = sandbox
            .exec(&handle, &["echo".into(), "hello".into()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout, "hello\n");
        assert_eq!(out.stderr, "oops\n");

        sandbox.destroy(handle).await.unwrap();

        let seen = stub.requests();
        assert!(
            seen.iter().any(|l| l.contains("/containers/create")),
            "create: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|l| l.contains("/containers/") && l.contains("/start")),
            "start: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|l| l.contains("/containers/") && l.contains("/exec")),
            "exec: {seen:?}"
        );
        assert!(
            seen.iter().any(|l| l.ends_with("/json")),
            "inspect: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|l| l.starts_with("DELETE ") && l.contains("/containers/")),
            "remove: {seen:?}"
        );
    }

    fn connect_stub(endpoint: &str) -> std::net::TcpStream {
        std::net::TcpStream::connect(endpoint.trim_start_matches("http://")).unwrap()
    }

    #[test]
    fn engine_stub_answers_unknown_routes_with_404() {
        use std::io::{Read, Write};
        let stub = EngineStub::start();
        let mut sock = connect_stub(&stub.endpoint);
        sock.write_all(b"GET /v1.0/nope HTTP/1.1\r\nHost: stub\r\n\r\n")
            .unwrap();
        let mut out = String::new();
        sock.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 404"), "{out}");
        assert_eq!(stub.requests(), vec!["GET /v1.0/nope".to_string()]);
    }

    #[test]
    fn engine_stub_survives_a_client_that_hangs_up_early() {
        use std::io::{Read, Write};
        let stub = EngineStub::start();
        // Connected and closed without a request: nothing is recorded.
        drop(connect_stub(&stub.endpoint));
        // A body shorter than its declared length ends the drain loop.
        let mut sock = connect_stub(&stub.endpoint);
        sock.write_all(
            b"POST /v1.0/containers/create HTTP/1.1\r\nHost: stub\r\nContent-Length: 100\r\n\r\nonly-ten!!",
        )
        .unwrap();
        sock.flush().unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        sock.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 201"), "{out}");
        assert_eq!(stub.requests().len(), 1);

        // A body larger than one read chunk needs a second read.
        let mut sock = connect_stub(&stub.endpoint);
        let body = "x".repeat(6000);
        let req = format!(
            "POST /v1.0/containers/create HTTP/1.1\r\nHost: stub\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(req.as_bytes()).unwrap();
        sock.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        sock.read_to_string(&mut out).unwrap();
        assert!(out.starts_with("HTTP/1.1 201"), "{out}");
        assert_eq!(stub.requests().len(), 2);
    }

    #[tokio::test]
    async fn docker_spawn_surfaces_create_and_start_errors() {
        let create = EngineStub::start_failing(Route::CreateContainer);
        let err = create
            .sandbox()
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Spawn(_)), "{err}");

        let start = EngineStub::start_failing(Route::StartContainer);
        let err = start
            .sandbox()
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Spawn(_)), "{err}");
    }

    #[tokio::test]
    async fn docker_exec_surfaces_create_and_start_errors() {
        let handle = SandboxHandle { id: "c".into() };
        let create = EngineStub::start_failing(Route::CreateExec);
        let err = create
            .sandbox()
            .exec(&handle, &["true".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Exec(_)), "{err}");

        let start = EngineStub::start_failing(Route::StartExec);
        let err = start
            .sandbox()
            .exec(&handle, &["true".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Exec(_)), "{err}");
    }

    #[tokio::test]
    async fn docker_exec_tolerates_an_uninspectable_exec() {
        let stub = EngineStub::start_failing(Route::InspectExec);
        let out = stub
            .sandbox()
            .exec(&SandboxHandle { id: "c".into() }, &["true".into()])
            .await
            .unwrap();
        assert_eq!(out.exit_code, -1, "unknown exit code must degrade to -1");
    }

    #[tokio::test]
    async fn docker_exec_surfaces_stream_errors() {
        let stub = EngineStub::start_truncating();
        let err = stub
            .sandbox()
            .exec(&SandboxHandle { id: "c".into() }, &["true".into()])
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Exec(_)), "{err}");
    }

    #[tokio::test]
    async fn docker_destroy_surfaces_errors() {
        let stub = EngineStub::start_failing(Route::RemoveContainer);
        let err = stub
            .sandbox()
            .destroy(SandboxHandle { id: "c".into() })
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Exec(_)), "{err}");
    }
}
