//! dsh runtime lifecycle: spawn, handshake, prompt, and per-session pooling.
//!
//! Why a pool: `initialize` has a 30 s handshake bound in dsh's own SDK, so a
//! cold start per turn is not viable. One runtime is reused per **session**, and
//! the session itself lives in `DSH_HOME`, so an evicted runtime is resumed by
//! the next `session/prompt` instead of losing context.
//!
//! Isolation (AGENTS rule 7): the harness home and workspace are derived from
//! the session id under a caller-provided root. Nothing is shared mutably across
//! sessions and `~/.dsh` is never discovered implicitly (dsh requires an explicit
//! `DSH_HOME`, and so do we).

use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, Mutex};

use agent_core::CoreError;

use crate::rpc::{
    RpcClient, ServerRequestHandler, UnsupportedRequests, DEFAULT_NOTIFICATION_CAPACITY,
    METHOD_INITIALIZE, METHOD_SESSION_PROMPT,
};
use crate::sandbox::DshSandbox;
use crate::transport::{RpcTransport, StdioTransport, DEFAULT_MAX_FRAME_BYTES};

/// dsh `sdk` profile flag (`dsh --profile sdk` owns the JSON-RPC server).
pub const SDK_PROFILE: &str = "sdk";
/// Provider route shipped with `deepseek-harness-sdk` 0.2.x.
pub const DEFAULT_DSH_PROVIDER: &str = "deepseek-official";
/// Permission presets the `sdk` profile accepts.
pub const PERMISSION_MODES: [&str; 3] = ["read-only", "workspace-write", "danger-full-access"];

fn provider_from_env(value: Option<String>) -> String {
    value
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_DSH_PROVIDER.to_string())
}

pub(crate) fn dsh_base_url(base_url: &str) -> String {
    let base = base_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return String::new();
    }
    if base.ends_with("/anthropic/v1") {
        return base.to_string();
    }
    let root = base.strip_suffix("/v1").unwrap_or(base);
    format!("{root}/anthropic/v1")
}

fn prompt_params(session: &str, input: &str) -> Value {
    json!({
        "sessionId": session,
        "contentBlocks": [{"type": "text", "text": input}],
    })
}

/// Everything needed to start one harness runtime.
#[derive(Debug, Clone)]
pub struct DshConfig {
    /// Executable (defaults to `DSH_BIN`, else `dsh` on `PATH`).
    pub bin: String,
    /// Harness home — **must** be per-session; never `~/.dsh`.
    pub home: PathBuf,
    /// Agent workspace (the child's cwd).
    pub cwd: PathBuf,
    /// Provider route inside dsh (defaults to [`DEFAULT_DSH_PROVIDER`]).
    pub provider: String,
    /// Model id inside that provider.
    pub model: String,
    /// dsh permission preset.
    pub permission_mode: String,
    /// Handshake bound (dsh's own default is 30 s).
    pub init_timeout: Duration,
    /// Per-request bound.
    pub request_timeout: Duration,
    /// Maximum accepted frame size.
    pub max_frame_bytes: usize,
    /// Extra environment entries (allow-listed; the child never inherits).
    pub extra_env: Vec<(String, String)>,
    /// Shared skill catalog to mirror into the workspace. `None` resolves
    /// [`agent_core::seams::shared_skill_catalog`] (the same directory the
    /// `skills` tool family and RSI's sink use); tests and non-standard
    /// deployments pin it explicitly.
    pub skills: Option<PathBuf>,
    /// Deployment-owned root the per-session directories are derived from.
    ///
    /// Kept explicitly so `acquire` can re-derive a session's `home`/`cwd`
    /// without nesting `.aria/dsh` a second time.
    pub session_root: PathBuf,
    /// Session this config belongs to (drives the volume/bind name).
    pub session: String,
    /// Run the harness in a sandbox container instead of on the host.
    pub sandbox: Option<DshSandbox>,
}

impl DshConfig {
    /// Build a config for one session under `root`.
    ///
    /// `root` is a deployment-owned directory (the sandbox workspace); the
    /// per-session subdirectory is derived deterministically so a resumed
    /// session finds its own home again.
    pub fn for_session(root: &Path, session: &str, model: &str) -> Self {
        let base = Self {
            bin: std::env::var("DSH_BIN").unwrap_or_else(|_| "dsh".to_string()),
            home: PathBuf::new(),
            cwd: PathBuf::new(),
            provider: provider_from_env(std::env::var("DSH_PROVIDER").ok()),
            model: model.to_string(),
            permission_mode: std::env::var("DSH_PERMISSION_MODE")
                .unwrap_or_else(|_| "workspace-write".to_string()),
            init_timeout: Duration::from_secs(30),
            request_timeout: Duration::from_secs(120),
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            extra_env: Vec::new(),
            skills: None,
            session_root: root.to_path_buf(),
            session: session.to_string(),
            sandbox: None,
        };
        base.for_session_like(session)
    }

    /// Clone this config for `session`, re-deriving only the paths.
    ///
    /// Every other setting (provider, timeouts, sandbox wiring, skill catalog) is
    /// inherited: the pool used to rebuild the config from scratch, which dropped
    /// the mounted catalog and doubled the `.aria/dsh` prefix.
    pub fn for_session_like(&self, session: &str) -> Self {
        let mut cfg = self.clone();
        cfg.session = session.to_string();
        match &cfg.sandbox {
            // Inside a container both paths live on the session volume, so they
            // are the same for every session (the volume carries identity).
            Some(sandbox) => {
                let (home, cwd) = sandbox.paths();
                cfg.home = home;
                cfg.cwd = cwd;
            }
            None => {
                let leaf = session_dir_name(session);
                cfg.home = cfg.session_root.join(".aria/dsh").join(&leaf);
                cfg.cwd = cfg.session_root.join(".aria/dsh-workspaces").join(leaf);
            }
        }
        cfg
    }

    /// Run this session in a sandbox container.
    ///
    /// Also re-derives `home`/`cwd` to the container's volume paths: leaving the
    /// host paths behind would point the harness at a directory it cannot see.
    pub fn with_sandbox(mut self, sandbox: DshSandbox) -> Self {
        let (home, cwd) = sandbox.paths();
        self.home = home;
        self.cwd = cwd;
        self.sandbox = Some(sandbox);
        self
    }

    /// Override the executable (tests / non-standard installs).
    pub fn with_bin(mut self, bin: impl Into<String>) -> Self {
        self.bin = bin.into();
        self
    }

    /// Append an environment entry passed to the child.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_env.push((key.into(), value.into()));
        self
    }

    /// Bridge the deployment's unified `GATEWAY_*` config into the names the
    /// external dsh binary reads in its allow-listed child environment.
    ///
    /// The shared `GATEWAY_BASE_URL` is the gateway root; dsh needs the
    /// Anthropic Messages prefix while the native loop uses the OpenAI one.
    pub fn with_gateway_env(mut self, base_url: &str, api_key: &str) -> Self {
        let base_url = dsh_base_url(base_url);
        if !base_url.is_empty() {
            self = self.with_env("DEEPSEEK_BASE_URL", &base_url);
        }
        if !api_key.is_empty() {
            self = self.with_env("DEEPSEEK_API_KEY", api_key);
        }
        self
    }

    /// Replace the permission preset.
    pub fn with_permission_mode(mut self, mode: impl Into<String>) -> Self {
        self.permission_mode = mode.into();
        self
    }

    /// Fail closed on anything dsh would reject only after a 30 s handshake.
    pub fn validate(&self) -> Result<(), CoreError> {
        if !PERMISSION_MODES.contains(&self.permission_mode.as_str()) {
            return Err(CoreError::Config(format!(
                "unknown dsh permission mode: {} (expected {})",
                self.permission_mode,
                PERMISSION_MODES.join("|")
            )));
        }
        if self.home.as_os_str().is_empty() {
            return Err(CoreError::Config(
                "dsh home is empty: set DSH_HOME or pass a session root".into(),
            ));
        }
        if self.model.trim().is_empty() {
            return Err(CoreError::Config("dsh model is empty".into()));
        }
        if let Some(sandbox) = &self.sandbox {
            if sandbox.image.trim().is_empty() {
                return Err(CoreError::Config(
                    "dsh sandbox image is empty (set DSH_SANDBOX_IMAGE)".into(),
                ));
            }
            if sandbox.bin.trim().is_empty() {
                return Err(CoreError::Config(
                    "dsh sandbox executable is empty (set DSH_SANDBOX_BIN)".into(),
                ));
            }
            if !sandbox.root.starts_with('/') {
                return Err(CoreError::Config(format!(
                    "dsh sandbox root must be absolute, got `{}`",
                    sandbox.root
                )));
            }
        }
        Ok(())
    }

    /// Pin the shared skill catalog instead of resolving it from the process
    /// environment (tests, non-standard installs).
    pub fn with_skills_catalog(mut self, catalog: impl Into<PathBuf>) -> Self {
        self.skills = Some(catalog.into());
        self
    }

    /// The shared skill catalog this session should see.
    ///
    /// One catalog for every consumer: `FilesystemSkills` reads it, RSI's sink
    /// writes it, and [`link_skill_catalog`] mirrors it into the workspace at
    /// `<cwd>/.aria/skills` where dsh looks. Resolution lives in
    /// [`agent_core::seams::shared_skill_catalog`] so the three cannot drift
    /// apart (ADR-0026).
    pub fn skill_catalog(&self) -> PathBuf {
        self.skills
            .clone()
            .unwrap_or_else(agent_core::seams::shared_skill_catalog)
    }

    /// The child environment: an allow-list, never the whole process env.
    ///
    /// Only `DSH_HOME`, `PATH` (so a bare `dsh` resolves) and the explicit
    /// [`DshConfig::with_env`] entries are forwarded. Provider credentials are
    /// **not** inherited implicitly: the deployment decides what the harness may
    /// see, which is also what keeps unrelated secrets out of it.
    fn child_env(&self) -> Vec<(String, String)> {
        // Defaults first, explicit entries last: a deployment that sets `PATH`
        // or `DSH_HOME` through `with_env` always wins.
        let mut env = vec![
            (
                "DSH_HOME".to_string(),
                self.home.to_string_lossy().to_string(),
            ),
            // Always set `PATH` (possibly empty) so a bare `dsh` resolves exactly
            // as the deployment intends and never inherits an unrelated path.
            (
                "PATH".to_string(),
                std::env::var("PATH").unwrap_or_default(),
            ),
        ];
        env.extend(self.extra_env.iter().cloned());
        env
    }

    /// The **container** environment: the image's `PATH`, never the host's.
    pub(crate) fn container_env(&self) -> Vec<(String, String)> {
        // Derived from the sandbox mount when present so a caller that forgot to
        // re-derive `home` cannot hand dsh a host path.
        let home = match &self.sandbox {
            Some(sandbox) => sandbox.paths().0,
            None => self.home.clone(),
        };
        let mut env = vec![
            ("DSH_HOME".to_string(), home.to_string_lossy().to_string()),
            (
                "PATH".to_string(),
                crate::sandbox::CONTAINER_PATH.to_string(),
            ),
        ];
        env.extend(self.extra_env.iter().cloned());
        env
    }
}

/// Mirror the shared skill catalog into a session workspace.
///
/// Best effort by design: a session must still start when the catalog cannot be
/// linked, because a missing skill degrades capability rather than invalidating a
/// turn. dsh resolves skills from `<cwd>/.aria/skills`, so mirroring the shared
/// catalog there is the whole integration — aria's own tool family reads the
/// catalog directly and needs no link.
fn link_skill_catalog(cfg: &DshConfig) {
    let catalog = cfg.skill_catalog();
    if !catalog.is_dir() {
        // Nothing distilled yet: an empty directory would only make
        // `skill_list` look configured when it is not.
        return;
    }
    let parent = cfg.cwd.join(".aria");
    let link = parent.join("skills");
    if std::fs::symlink_metadata(&link).is_ok() {
        return;
    }
    if let Err(e) = prepare_link(&catalog, &parent, &link) {
        tracing::warn!(catalog = %catalog.display(), error = %e, "cannot share the skill catalog with dsh");
    }
}

/// Create the parent directory and then the link itself.
fn prepare_link(catalog: &Path, parent: &Path, link: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(parent)?;
    link_dir(catalog, link)
}

#[cfg(unix)]
fn link_dir(catalog: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(catalog, link)
}

#[cfg(not(unix))]
fn link_dir(catalog: &Path, link: &Path) -> std::io::Result<()> {
    copy_tree(catalog, link)
}

#[cfg(not(unix))]
fn copy_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// A live harness runtime the loop can prompt.
#[async_trait]
pub trait HarnessRuntime: Send + Sync {
    /// Queue `input` into `session` and return the notification stream.
    async fn prompt(&self, session: &str, input: &str) -> Result<mpsc::Receiver<Value>, CoreError>;
    /// Terminate the runtime.
    async fn shutdown(&self) -> Result<(), CoreError>;
}

/// One `dsh --profile sdk` child process.
pub struct DshRuntime {
    client: RpcClient,
    cfg: DshConfig,
}

impl DshRuntime {
    /// Spawn the runtime and complete the `initialize` handshake.
    ///
    /// Two transports share one handshake: a host child process ([`StdioTransport`])
    /// or a container-attached process ([`crate::sandbox::SandboxTransport`]). The
    /// choice is `cfg.sandbox`, and there is no fallback — a sandboxed session that
    /// cannot get its container fails instead of silently running on the host.
    pub async fn connect(
        cfg: DshConfig,
        handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Self, CoreError> {
        cfg.validate()?;
        let transport: Arc<dyn RpcTransport> = match &cfg.sandbox {
            Some(sandbox) => Arc::new(crate::sandbox::sandbox_transport(&cfg, sandbox).await?)
                as Arc<dyn RpcTransport>,
            None => Arc::new(host_transport(&cfg)?) as Arc<dyn RpcTransport>,
        };
        Self::handshake(cfg, transport, handler).await
    }

    async fn handshake(
        cfg: DshConfig,
        transport: Arc<dyn RpcTransport>,
        handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Self, CoreError> {
        let client = RpcClient::new(transport);
        let arc = Arc::new(client.clone());
        arc.start_reader(handler);
        let init = client
            .call(
                METHOD_INITIALIZE,
                json!({
                    "cwd": cfg.cwd.to_string_lossy(),
                    "provider": cfg.provider,
                    "model": cfg.model,
                    "approval_requests": true,
                    "permission_mode": cfg.permission_mode,
                }),
                cfg.init_timeout,
            )
            .await;
        if let Err(e) = init {
            // A container whose handshake failed must not keep billing.
            let _ = client.shutdown().await;
            return Err(e);
        }
        tracing::debug!(model = %cfg.model, provider = %cfg.provider, "dsh runtime initialized");
        Ok(Self { client, cfg })
    }
}

/// Host transport: a real child process in a workspace this process owns.
fn host_transport(cfg: &DshConfig) -> Result<StdioTransport, CoreError> {
    std::fs::create_dir_all(&cfg.home).map_err(|e| {
        CoreError::Config(format!(
            "cannot create dsh home {}: {e}",
            cfg.home.display()
        ))
    })?;
    std::fs::create_dir_all(&cfg.cwd).map_err(|e| {
        CoreError::Config(format!(
            "cannot create dsh workspace {}: {e}",
            cfg.cwd.display()
        ))
    })?;
    // dsh runs tools in its own workspace, so a skill distilled by RSI has to
    // be visible *there*, not only in the aria sandbox root.
    link_skill_catalog(cfg);
    let args = vec!["--profile".to_string(), SDK_PROFILE.to_string()];
    StdioTransport::spawn(
        &cfg.bin,
        &args,
        &cfg.child_env(),
        &cfg.cwd,
        cfg.max_frame_bytes,
    )
}

#[async_trait]
impl HarnessRuntime for DshRuntime {
    async fn prompt(&self, session: &str, input: &str) -> Result<mpsc::Receiver<Value>, CoreError> {
        // Subscribe first: `session/prompt` may start emitting immediately.
        let rx = self.client.subscribe(DEFAULT_NOTIFICATION_CAPACITY).await;
        self.client
            .call(
                METHOD_SESSION_PROMPT,
                prompt_params(session, input),
                self.cfg.request_timeout,
            )
            .await?;
        Ok(rx)
    }

    async fn shutdown(&self) -> Result<(), CoreError> {
        self.client.shutdown().await
    }
}

/// Creates a runtime for a config (injectable so the pool is testable offline).
#[async_trait]
pub trait RuntimeConnector: Send + Sync {
    async fn connect(
        &self,
        cfg: DshConfig,
        handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Arc<dyn HarnessRuntime>, CoreError>;
}

/// Production connector: spawns the real child process.
pub struct DshConnector;

#[async_trait]
impl RuntimeConnector for DshConnector {
    async fn connect(
        &self,
        cfg: DshConfig,
        handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Arc<dyn HarnessRuntime>, CoreError> {
        Ok(Arc::new(DshRuntime::connect(cfg, handler).await?))
    }
}

struct Entry {
    runtime: Arc<dyn HarnessRuntime>,
    last_used: Instant,
}

/// Per-session runtime pool with a hard capacity and an idle reaper.
pub struct DshPool {
    cfg: DshConfig,
    max: usize,
    idle_ttl: Duration,
    handler: Arc<dyn ServerRequestHandler>,
    connector: Arc<dyn RuntimeConnector>,
    entries: Mutex<HashMap<String, Entry>>,
}

impl DshPool {
    /// Build a pool whose session homes live under `cfg.home`'s parent.
    pub fn new(
        cfg: DshConfig,
        max: usize,
        idle_ttl: Duration,
        handler: Arc<dyn ServerRequestHandler>,
    ) -> Self {
        Self::with_connector(cfg, max, idle_ttl, handler, Arc::new(DshConnector))
    }

    /// Build with an injected connector (tests).
    pub fn with_connector(
        cfg: DshConfig,
        max: usize,
        idle_ttl: Duration,
        handler: Arc<dyn ServerRequestHandler>,
        connector: Arc<dyn RuntimeConnector>,
    ) -> Self {
        Self {
            cfg,
            max: max.max(1),
            idle_ttl,
            handler,
            connector,
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// Borrow the runtime for `session`, starting it on demand.
    pub async fn acquire(&self, session: &str) -> Result<Arc<dyn HarnessRuntime>, CoreError> {
        {
            let mut entries = self.entries.lock().await;
            if let Some(entry) = entries.get_mut(session) {
                entry.last_used = Instant::now();
                return Ok(entry.runtime.clone());
            }
            if entries.len() >= self.max {
                return Err(CoreError::Config(format!(
                    "dsh pool is full ({} runtimes); retry shortly",
                    self.max
                )));
            }
        }
        let mut cfg = self.cfg.for_session_like(session);
        cfg.session = session.to_string();
        let runtime = self.connector.connect(cfg, self.handler.clone()).await?;
        let mut entries = self.entries.lock().await;
        entries.insert(
            session.to_string(),
            Entry {
                runtime: runtime.clone(),
                last_used: Instant::now(),
            },
        );
        Ok(runtime)
    }

    /// Drop (and shut down) runtimes idle for longer than the TTL.
    pub async fn sweep(&self) -> usize {
        let mut expired = Vec::new();
        {
            let mut entries = self.entries.lock().await;
            let drained: Vec<(String, Entry)> = entries.drain().collect();
            for (id, entry) in drained {
                if entry.last_used.elapsed() >= self.idle_ttl {
                    expired.push(entry.runtime);
                } else {
                    entries.insert(id, entry);
                }
            }
        }
        let count = expired.len();
        for runtime in expired {
            let _ = runtime.shutdown().await;
        }
        count
    }

    /// Number of live runtimes (diagnostics / tests).
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    /// Whether no runtime is live.
    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }
}

/// Filesystem-safe leaf for a session id (`::sub::` and slashes included).
pub fn session_dir_name(session: &str) -> String {
    let mut out: String = session
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    out.truncate(96);
    if out.is_empty() {
        out.push_str("default");
    }
    out
}

/// Build a runtime that refuses every server request (used when no bridge is
/// wired — approvals then fail closed at the JSON-RPC layer).
pub fn no_bridge() -> Arc<dyn ServerRequestHandler> {
    Arc::new(UnsupportedRequests)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn session_names_are_filesystem_safe() {
        assert_eq!(session_dir_name("abc-123_X"), "abc-123_X");
        assert_eq!(session_dir_name("p::sub::9"), "p__sub__9");
        assert_eq!(session_dir_name(""), "default");
        assert_eq!(session_dir_name(&"a".repeat(200)).len(), 96);
        assert_eq!(session_dir_name("../../etc"), "______etc");
    }

    fn sandboxed_cfg() -> DshConfig {
        let sandbox = crate::sandbox::DshSandbox {
            sandbox: std::sync::Arc::new(agent_sandbox::DockerSandbox::new()),
            image: "aria-dsh-sandbox:latest".into(),
            mount: crate::sandbox::SandboxMount::Volume,
            root: "/aria/session".into(),
            bin: "/usr/local/bin/dsh".into(),
            volume_prefix: "aria-dsh".into(),
            resources: agent_sandbox::SandboxResourceLimits::default(),
            hardening: agent_sandbox::SandboxHardening::default(),
            network: None,
            labels: Vec::new(),
        };
        DshConfig::for_session(Path::new("/srv"), "s1", "m").with_sandbox(sandbox)
    }

    #[test]
    fn a_sandboxed_config_rejects_an_empty_image_bin_or_relative_root() {
        let cfg = sandboxed_cfg();
        cfg.validate().expect("valid");

        let mut empty_image = cfg.clone();
        empty_image.sandbox.as_mut().unwrap().image = "  ".into();
        let err = empty_image.validate().unwrap_err().to_string();
        assert!(err.contains("image is empty"), "{err}");

        let mut empty_bin = cfg.clone();
        empty_bin.sandbox.as_mut().unwrap().bin = String::new();
        let err = empty_bin.validate().unwrap_err().to_string();
        assert!(err.contains("executable is empty"), "{err}");

        let mut relative = cfg.clone();
        relative.sandbox.as_mut().unwrap().root = "aria/session".into();
        let err = relative.validate().unwrap_err().to_string();
        assert!(err.contains("must be absolute"), "{err}");
    }

    #[test]
    fn the_container_environment_falls_back_to_the_host_home_without_a_sandbox() {
        let cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m");
        let env = cfg.container_env();
        let (key, home) = env.iter().find(|(key, _)| key == "DSH_HOME").unwrap();
        assert_eq!(key, "DSH_HOME");
        assert_eq!(*home, cfg.home.to_string_lossy());
    }

    #[test]
    fn config_derives_isolated_paths_per_session() {
        let cfg = DshConfig::for_session(Path::new("/srv"), "tenant-a::s1", "deepseek-v4");
        assert_eq!(cfg.model, "deepseek-v4");
        assert!(cfg.home.ends_with("tenant-a__s1"));
        assert!(cfg.cwd.ends_with("tenant-a__s1"));
        assert_ne!(cfg.home, cfg.cwd);
        assert_eq!(cfg.permission_mode, "workspace-write");
        cfg.validate().expect("valid");
    }

    #[test]
    fn the_skill_catalog_defaults_to_the_shared_catalog_and_can_be_pinned() {
        let cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m");
        assert!(cfg.skills.is_none(), "unpinned resolves at call time");
        assert_eq!(
            cfg.skill_catalog(),
            agent_core::seams::shared_skill_catalog()
        );
        let pinned = cfg.with_skills_catalog("/srv/shared/skills");
        assert_eq!(pinned.skill_catalog(), PathBuf::from("/srv/shared/skills"));
    }

    #[test]
    fn a_distilled_skill_reaches_the_dsh_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let catalog = dir.path().join(".aria/skills/rsi-retry-storm-shell");
        std::fs::create_dir_all(&catalog).unwrap();
        std::fs::write(catalog.join("SKILL.md"), "# stop retrying").unwrap();

        let cfg = DshConfig::for_session(dir.path(), "s1", "m")
            .with_skills_catalog(dir.path().join(".aria/skills"));
        std::fs::create_dir_all(&cfg.cwd).unwrap();
        link_skill_catalog(&cfg);
        let shared = cfg.cwd.join(".aria/skills");
        assert!(shared.is_dir());
        assert_eq!(
            std::fs::read_to_string(shared.join("rsi-retry-storm-shell/SKILL.md")).unwrap(),
            "# stop retrying"
        );

        // Re-linking is a no-op rather than an error.
        link_skill_catalog(&cfg);
        assert!(shared.is_dir());
    }

    #[test]
    fn linking_without_a_catalog_or_without_a_parent_is_silent() {
        let empty = tempfile::tempdir().unwrap();
        let cfg = DshConfig::for_session(empty.path(), "s1", "m")
            .with_skills_catalog(empty.path().join("missing"));
        std::fs::create_dir_all(&cfg.cwd).unwrap();
        link_skill_catalog(&cfg);
        assert!(!cfg.cwd.join(".aria/skills").exists());

        // A `.aria` file blocks the directory: the failure is logged, not fatal.
        let blocked = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(blocked.path().join(".aria/skills")).unwrap();
        let cfg = DshConfig::for_session(blocked.path(), "s1", "m")
            .with_skills_catalog(blocked.path().join(".aria/skills"));
        std::fs::create_dir_all(&cfg.cwd).unwrap();
        std::fs::write(cfg.cwd.join(".aria"), "not a directory").unwrap();
        link_skill_catalog(&cfg);
        assert!(cfg.cwd.join(".aria").is_file());
    }

    #[test]
    fn prompt_payload_uses_content_blocks() {
        let params = prompt_params("s1", "hello");
        assert_eq!(params["sessionId"], "s1");
        assert_eq!(params["contentBlocks"][0]["type"], "text");
        assert_eq!(params["contentBlocks"][0]["text"], "hello");
        assert!(params.get("blocks").is_none());
    }

    #[test]
    fn dsh_base_url_uses_anthropic_prefix() {
        assert_eq!(dsh_base_url(""), "");
        assert_eq!(
            dsh_base_url("https://gateway.example"),
            "https://gateway.example/anthropic/v1"
        );
        assert_eq!(
            dsh_base_url("https://gateway.example/v1/"),
            "https://gateway.example/anthropic/v1"
        );
        assert_eq!(
            dsh_base_url("https://gateway.example/anthropic/v1"),
            "https://gateway.example/anthropic/v1"
        );
    }

    #[test]
    fn provider_defaults_to_registered_dsh_adapter() {
        assert_eq!(provider_from_env(None), "deepseek-official");
        assert_eq!(provider_from_env(Some("  ".into())), "deepseek-official");
        assert_eq!(
            provider_from_env(Some("custom-adapter".into())),
            "custom-adapter"
        );
    }

    #[test]
    fn config_validation_is_fail_closed() {
        let mut cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m");
        cfg.permission_mode = "root".into();
        assert!(cfg.validate().is_err());
        let mut cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m");
        cfg.model = "  ".into();
        assert!(cfg.validate().is_err());
        let mut cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m");
        cfg.home = PathBuf::new();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn builders_and_env_allow_list() {
        let cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m")
            .with_bin("/opt/dsh")
            .with_env("DSH_X", "1")
            .with_permission_mode("read-only");
        assert_eq!(cfg.bin, "/opt/dsh");
        assert_eq!(cfg.permission_mode, "read-only");
        let env = cfg.child_env();
        assert!(env.iter().any(|(k, v)| k == "DSH_X" && v == "1"));
        assert!(env.iter().any(|(k, _)| k == "DSH_HOME"));
        assert!(env.iter().any(|(k, _)| k == "PATH"));
    }

    #[test]
    fn gateway_config_is_mapped_into_the_child_allow_list() {
        let cfg = DshConfig::for_session(Path::new("/srv"), "s1", "m")
            .with_gateway_env("https://gateway.example", "gw-key");
        assert_eq!(
            cfg.extra_env,
            vec![
                (
                    "DEEPSEEK_BASE_URL".to_string(),
                    "https://gateway.example/anthropic/v1".to_string()
                ),
                ("DEEPSEEK_API_KEY".to_string(), "gw-key".to_string()),
            ]
        );
        let env = cfg.child_env();
        assert!(env
            .iter()
            .any(|(k, v)| k == "DEEPSEEK_BASE_URL" && v == "https://gateway.example/anthropic/v1"));
        assert!(env
            .iter()
            .any(|(k, v)| k == "DEEPSEEK_API_KEY" && v == "gw-key"));

        let empty = DshConfig::for_session(Path::new("/srv"), "s1", "m").with_gateway_env("", "");
        assert!(empty.extra_env.is_empty());
    }

    /// Fake runtime that counts prompts and shutdowns.
    struct FakeRuntime {
        prompts: AtomicUsize,
        shutdowns: AtomicUsize,
    }

    #[async_trait]
    impl HarnessRuntime for FakeRuntime {
        async fn prompt(
            &self,
            _session: &str,
            _input: &str,
        ) -> Result<mpsc::Receiver<Value>, CoreError> {
            self.prompts.fetch_add(1, Ordering::SeqCst);
            let (_tx, rx) = mpsc::channel(1);
            Ok(rx)
        }

        async fn shutdown(&self) -> Result<(), CoreError> {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct CountingConnector {
        calls: AtomicUsize,
        homes: Mutex<Vec<PathBuf>>,
        fail: bool,
    }

    #[async_trait]
    impl RuntimeConnector for CountingConnector {
        async fn connect(
            &self,
            cfg: DshConfig,
            _handler: Arc<dyn ServerRequestHandler>,
        ) -> Result<Arc<dyn HarnessRuntime>, CoreError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.homes.lock().await.push(cfg.home.clone());
            if self.fail {
                return Err(CoreError::Config("connect refused".into()));
            }
            Ok(Arc::new(FakeRuntime {
                prompts: AtomicUsize::new(0),
                shutdowns: AtomicUsize::new(0),
            }))
        }
    }

    fn pool(connector: Arc<CountingConnector>, max: usize, ttl: Duration) -> DshPool {
        let cfg = DshConfig::for_session(Path::new("/srv"), "seed", "m");
        DshPool::with_connector(cfg, max, ttl, no_bridge(), connector)
    }

    #[tokio::test]
    async fn the_pool_reuses_one_runtime_per_session() {
        let connector = Arc::new(CountingConnector {
            calls: AtomicUsize::new(0),
            homes: Mutex::new(Vec::new()),
            fail: false,
        });
        let pool = pool(connector.clone(), 2, Duration::from_secs(60));
        assert!(pool.is_empty().await);
        let first = pool.acquire("s1").await.expect("runtime");
        let again = pool.acquire("s1").await.expect("runtime");
        assert!(Arc::ptr_eq(&first, &again));
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
        assert_eq!(pool.len().await, 1);
        let other = pool.acquire("s2").await.expect("runtime");
        assert!(!Arc::ptr_eq(&first, &other));
        let homes = connector.homes.lock().await;
        assert!(homes[0].ends_with("s1"));
        assert!(homes[1].ends_with("s2"));
        assert_ne!(homes[0], homes[1]);
        // A long TTL keeps both entries: nothing is reaped.
        assert_eq!(pool.sweep().await, 0);
        assert_eq!(pool.len().await, 2);
    }

    #[tokio::test]
    async fn the_pool_fails_closed_when_full() {
        let connector = Arc::new(CountingConnector {
            calls: AtomicUsize::new(0),
            homes: Mutex::new(Vec::new()),
            fail: false,
        });
        let pool = pool(connector, 1, Duration::from_secs(60));
        pool.acquire("s1").await.expect("runtime");
        let err = pool.acquire("s2").await.err().expect("pool full");
        assert!(err.to_string().contains("pool is full"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_connect_is_surfaced() {
        let connector = Arc::new(CountingConnector {
            calls: AtomicUsize::new(0),
            homes: Mutex::new(Vec::new()),
            fail: true,
        });
        let pool = pool(connector, 2, Duration::from_secs(60));
        assert!(pool.acquire("s1").await.is_err());
        assert!(pool.is_empty().await);
    }

    #[tokio::test]
    async fn unusable_directories_are_actionable_config_errors() {
        let dir = tempfile::tempdir().expect("tempdir");
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, "x").expect("write file");

        let mut cfg = DshConfig::for_session(dir.path(), "s1", "m");
        cfg.home = file.clone();
        let err = DshRuntime::connect(cfg, no_bridge())
            .await
            .err()
            .expect("home must fail");
        assert!(err.to_string().contains("cannot create dsh home"), "{err}");

        let mut cfg = DshConfig::for_session(dir.path(), "s1", "m");
        cfg.cwd = file;
        let err = DshRuntime::connect(cfg, no_bridge())
            .await
            .err()
            .expect("workspace must fail");
        assert!(
            err.to_string().contains("cannot create dsh workspace"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_home_without_a_parent_falls_back_to_itself() {
        let connector = Arc::new(CountingConnector {
            calls: AtomicUsize::new(0),
            homes: Mutex::new(Vec::new()),
            fail: false,
        });
        let mut cfg = DshConfig::for_session(Path::new("/srv"), "seed", "m");
        // `Path::new("/").parent()` is `None`: exercise the defensive fallback.
        cfg.home = PathBuf::from("/");
        let pool = DshPool::with_connector(
            cfg,
            1,
            Duration::from_secs(60),
            no_bridge(),
            connector.clone(),
        );
        let runtime = pool.acquire("root-session").await.expect("runtime");
        assert_eq!(runtime.prompt("root-session", "hi").await.unwrap().len(), 0);
        runtime.shutdown().await.expect("shutdown");
        assert_eq!(connector.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_sweep_releases_idle_runtimes() {
        let connector = Arc::new(CountingConnector {
            calls: AtomicUsize::new(0),
            homes: Mutex::new(Vec::new()),
            fail: false,
        });
        let pool = pool(connector, 2, Duration::ZERO);
        pool.acquire("s1").await.expect("runtime");
        assert_eq!(pool.sweep().await, 1);
        assert!(pool.is_empty().await);
        assert_eq!(pool.sweep().await, 0);
    }
}
