//! Run a dsh session **inside a sandbox container** instead of on the host.
//!
//! Why: under `harness = dsh` the harness executes every tool call itself, so the
//! host process IS the tool boundary. Without a container the isolation is only
//! dsh's `workspace-write` preset — a policy check, not a filesystem boundary
//! (see `docs/adr/0027-dsh-sandbox-runtime.md`).
//!
//! Shape (documented in the ADR, decided against the alternatives):
//!
//! * **F2, a keep-alive container plus an attached `exec`.** `spawn` owns the
//!   container's lifetime; the harness runs as an attached process inside it.
//!   `start_exec`'s duplex stream is already the proven bollard path here, while
//!   `attach_container` decodes output with a newline decoder that is wrong for
//!   a non-tty `8`-byte framed stream.
//! * **A per-session named volume** mounted at [`DEFAULT_ROOT`]. Under DooD a
//!   bind mount is resolved on the daemon's host, so a control-plane path would
//!   silently point at nothing; the volume is the only cross-host-safe workspace.
//!   `DSH_HOME` and the agent cwd both live on it, so an evicted runtime resumes
//!   the same session.
//! * **The skill catalog is uploaded (tar), not symlinked.** The catalog lives on
//!   the control-plane filesystem, which the container cannot see.
//!
//! Everything fails closed: if the container, the workspace, the upload or the
//! handshake fails, the session fails and the container is destroyed. There is no
//! silent fallback to a host process — that would turn "sandboxed" into a claim.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agent_core::CoreError;
use agent_sandbox::{
    AttachedExec, AttachedSpec, ExecSpec, LogChunk, MountSpec, Sandbox, SandboxHandle,
    SandboxHardening, SandboxResourceLimits,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
#[cfg(test)]
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;

use crate::runtime::{session_dir_name, DshConfig, SDK_PROFILE};
use crate::transport::RpcTransport;

/// Where a session's files are mounted inside the container.
pub const DEFAULT_ROOT: &str = "/aria/session";
/// Container path `dsh` is installed at by the dedicated sandbox image.
pub const DEFAULT_BIN: &str = "/usr/local/bin/dsh";
/// `PATH` handed to the harness: the image's, never the host's.
pub const CONTAINER_PATH: &str = "/usr/local/bin:/usr/bin:/bin";

/// How the session directory is backed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMount {
    /// A daemon-owned named volume (the default; DooD-safe).
    Volume,
    /// A host directory — only correct when the control plane runs on the same
    /// filesystem as the daemon (local builds, `cargo run`).
    Bind,
}

impl SandboxMount {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "volume" => Some(SandboxMount::Volume),
            "bind" => Some(SandboxMount::Bind),
            _ => None,
        }
    }
}

/// Everything the sandboxed runtime needs that the host runtime does not.
#[derive(Clone)]
pub struct DshSandbox {
    /// Backend that owns the container.
    pub sandbox: Arc<dyn Sandbox>,
    /// Image with `dsh` installed (never pulled by this crate).
    pub image: String,
    /// Session-directory backing.
    pub mount: SandboxMount,
    /// Container path the session directory is mounted at.
    pub root: String,
    /// `dsh` executable **inside** the container.
    pub bin: String,
    /// Volume-name prefix (volumes are labelled so they can be pruned).
    pub volume_prefix: String,
    /// Resource budget for the harness container.
    pub resources: SandboxResourceLimits,
    /// Isolation hardening applied at container creation.
    pub hardening: SandboxHardening,
    /// Governed egress network (ADR-0024).
    pub network: Option<String>,
    /// Tenant/session identity labels read by the egress proxy.
    pub labels: Vec<(String, String)>,
}

impl fmt::Debug for DshSandbox {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DshSandbox")
            .field("image", &self.image)
            .field("mount", &self.mount)
            .field("root", &self.root)
            .field("bin", &self.bin)
            .field("network", &self.network)
            .finish_non_exhaustive()
    }
}

impl DshSandbox {
    /// Container paths a session uses on its volume.
    pub fn paths(&self) -> (PathBuf, PathBuf) {
        let root = Path::new(&self.root);
        (root.join("dsh"), root.join("cwd"))
    }

    /// Volume name (or bind source) for one session.
    ///
    /// Deterministic so a resumed session finds its own workspace again; the
    /// tenant is part of the name so two tenants can never collide.
    pub fn mount_for(&self, cfg: &DshConfig) -> Result<MountSpec, CoreError> {
        let leaf = session_dir_name(&cfg.session);
        let tenant = cfg
            .session_root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        match self.mount {
            SandboxMount::Volume => {
                let name = volume_name(&self.volume_prefix, &tenant, &leaf);
                Ok(MountSpec::volume(name, self.root.clone()))
            }
            SandboxMount::Bind => {
                let source = cfg.session_root.join(".aria/dsh").join(&leaf);
                std::fs::create_dir_all(&source).map_err(|e| {
                    CoreError::Config(format!(
                        "cannot create the dsh session workspace {}: {e}",
                        source.display()
                    ))
                })?;
                Ok(MountSpec::bind(
                    source.to_string_lossy().to_string(),
                    self.root.clone(),
                ))
            }
        }
    }
}

/// Docker volume names accept `[a-zA-Z0-9][a-zA-Z0-9_.-]*`; everything else is
/// folded to `-` so a tenant id can never produce an invalid or ambiguous name.
fn volume_name(prefix: &str, tenant: &str, leaf: &str) -> String {
    format!("{prefix}-{tenant}-{leaf}")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Pack the shared skill catalog as a tar whose top level is `skills/`, so it
/// extracts into `<cwd>/.aria` and lands where dsh looks.
pub fn skill_catalog_tar(catalog: &Path) -> Option<Vec<u8>> {
    if !catalog.is_dir() {
        return None;
    }
    let mut builder = tar::Builder::new(Vec::new());
    if builder.append_dir_all("skills", catalog).is_err() {
        return None;
    }
    builder.into_inner().ok()
}

/// One live duplex channel to the containerised harness.
///
/// `close` destroys the session container: the process is not the lifetime
/// owner of the sandbox, so killing the process alone could leave a container
/// (and its volume) billing behind.
pub struct SandboxTransport {
    stdin: Mutex<agent_sandbox::AttachedInput>,
    output: Mutex<agent_sandbox::AttachedOutput>,
    buffer: Mutex<String>,
    max_frame: usize,
    idle: Duration,
    teardown: Mutex<Option<Teardown>>,
}

/// What `close` has to tear down.
pub struct Teardown {
    sandbox: Arc<dyn Sandbox>,
    handle: SandboxHandle,
}

impl SandboxTransport {
    pub fn new(
        attached: AttachedExec,
        teardown: Option<Teardown>,
        max_frame: usize,
        idle: Duration,
    ) -> Self {
        Self {
            stdin: Mutex::new(attached.stdin),
            output: Mutex::new(attached.output),
            buffer: Mutex::new(String::new()),
            max_frame,
            idle,
            teardown: Mutex::new(teardown),
        }
    }

    /// Next stdout bytes, logging stderr (never model content) at debug level.
    async fn next_chunk(&self) -> Result<Option<String>, CoreError> {
        let mut output = self.output.lock().await;
        loop {
            match output.next().await {
                Some(Ok(LogChunk::Stdout(chunk))) => return Ok(Some(chunk)),
                Some(Ok(LogChunk::Stderr(chunk))) => {
                    tracing::debug!(stderr = %chunk, "dsh sandbox stderr");
                }
                Some(Err(e)) => {
                    return Err(CoreError::Model(format!(
                        "dsh sandbox transport read failed: {e}"
                    )))
                }
                None => return Ok(None),
            }
        }
    }
}

#[async_trait::async_trait]
impl RpcTransport for SandboxTransport {
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

    async fn recv_line(&self) -> Result<Option<String>, CoreError> {
        let mut buffer = self.buffer.lock().await;
        loop {
            if let Some(index) = buffer.find('\n') {
                let line: String = buffer.drain(..=index).collect();
                return Ok(Some(line.trim_end_matches(['\n', '\r']).to_string()));
            }
            if buffer.len() > self.max_frame {
                return Err(CoreError::Model(format!(
                    "dsh frame exceeds {} bytes (protocol error)",
                    self.max_frame
                )));
            }
            let next = tokio::time::timeout(self.idle, self.next_chunk()).await;
            match next {
                Err(_) => {
                    return Err(CoreError::Model(format!(
                        "dsh sandbox produced no output for {:?}",
                        self.idle
                    )))
                }
                Ok(Err(e)) => return Err(e),
                Ok(Ok(Some(chunk))) => buffer.push_str(&chunk),
                Ok(Ok(None)) => return Ok(None),
            }
        }
    }

    async fn close(&self) -> Result<(), CoreError> {
        let teardown = self.teardown.lock().await.take();
        if let Some(teardown) = teardown {
            // A container that is already gone is not an error: close is
            // idempotent by contract.
            if let Err(e) = teardown.sandbox.destroy(teardown.handle).await {
                tracing::debug!(error = %e, "dsh sandbox was already gone");
            }
        }
        Ok(())
    }
}

/// The session container's spawn spec.
///
/// `command` is unused by the Docker backend (the session idles on its own
/// keep-alive) but the seam requires one.
pub fn session_spec(cfg: &DshConfig, sandbox: &DshSandbox) -> Result<ExecSpec, CoreError> {
    let mut spec = ExecSpec::command(vec!["true".into()]);
    spec.image = Some(sandbox.image.clone());
    spec.env = cfg.container_env();
    spec.network = sandbox.network.clone();
    spec.labels = sandbox.labels.clone();
    spec.mounts = vec![sandbox.mount_for(cfg)?];
    spec.hardening = sandbox.hardening.clone();
    spec.resources = Some(sandbox.resources);
    Ok(spec)
}

/// Spawn the session container, prepare its workspace, and attach the harness.
///
/// Any failure after `spawn` destroys the container — a half-prepared session
/// must not keep billing.
pub async fn sandbox_transport(
    cfg: &DshConfig,
    sandbox: &DshSandbox,
) -> Result<SandboxTransport, CoreError> {
    let spec = session_spec(cfg, sandbox)?;
    let handle = sandbox
        .sandbox
        .spawn(&spec)
        .await
        .map_err(|e| CoreError::Config(format!("dsh sandbox spawn failed: {e}")))?;
    match prepare(cfg, sandbox, &handle).await {
        Ok(attached) => Ok(SandboxTransport::new(
            attached,
            Some(Teardown {
                sandbox: sandbox.sandbox.clone(),
                handle,
            }),
            cfg.max_frame_bytes,
            cfg.request_timeout,
        )),
        Err(e) => {
            if let Err(cleanup) = sandbox.sandbox.destroy(handle).await {
                tracing::warn!(error = %cleanup, "cannot destroy a failed dsh sandbox");
            }
            Err(e)
        }
    }
}

async fn prepare(
    cfg: &DshConfig,
    sandbox: &DshSandbox,
    handle: &SandboxHandle,
) -> Result<AttachedExec, CoreError> {
    let (home, cwd) = sandbox.paths();
    let aria_dir = cwd.join(".aria");
    // The volume is mounted at `root`; its subdirectories are not created by
    // Docker, and dsh refuses to invent its own home.
    let mkdir = vec![
        "mkdir".to_string(),
        "-p".to_string(),
        home.to_string_lossy().to_string(),
        cwd.to_string_lossy().to_string(),
        aria_dir.to_string_lossy().to_string(),
    ];
    let prepared = sandbox
        .sandbox
        .exec(handle, &mkdir)
        .await
        .map_err(|e| CoreError::Config(format!("dsh sandbox workspace: {e}")))?;
    if prepared.exit_code != 0 {
        return Err(CoreError::Config(format!(
            "dsh sandbox cannot prepare {}: exit={} stderr={}",
            sandbox.root, prepared.exit_code, prepared.stderr
        )));
    }
    if let Some(tar) = skill_catalog_tar(&cfg.skill_catalog()) {
        sandbox
            .sandbox
            .copy_in(handle, &aria_dir.to_string_lossy(), tar)
            .await
            .map_err(|e| CoreError::Config(format!("dsh sandbox skill catalog: {e}")))?;
    }
    let mut attached = AttachedSpec::command(vec![
        sandbox.bin.clone(),
        "--profile".to_string(),
        SDK_PROFILE.to_string(),
    ]);
    attached.workdir = Some(cwd.to_string_lossy().to_string());
    sandbox
        .sandbox
        .exec_attached(handle, &attached)
        .await
        .map_err(|e| CoreError::Config(format!("dsh sandbox cannot start the harness: {e}")))
}

/// Read every line of a duplex stream into `LogChunk::Stdout` frames.
///
/// Only used by tests, which need a scripted stand-in for a container.
#[cfg(test)]
fn line_stream<R>(reader: R) -> agent_sandbox::AttachedOutput
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    let stream = async_stream::stream! {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            yield Ok(LogChunk::Stdout(format!("{line}\n")));
        }
    };
    Box::pin(stream)
}

/// Yield raw byte chunks as they arrive (what Docker's raw stream really does,
/// including a frame that is only half a line).
#[cfg(test)]
fn byte_stream<R>(mut reader: R) -> agent_sandbox::AttachedOutput
where
    R: tokio::io::AsyncRead + Send + Unpin + 'static,
{
    let stream = async_stream::stream! {
        let mut buffer = vec![0u8; 64];
        loop {
            match tokio::io::AsyncReadExt::read(&mut reader, &mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => yield Ok(LogChunk::Stdout(
                    String::from_utf8_lossy(&buffer[..n]).to_string(),
                )),
            }
        }
    };
    Box::pin(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{DshConfig, DshPool, HarnessRuntime};
    use crate::transport::RpcTransport;
    use agent_sandbox::{ExecOutput, MountKind, SandboxError};
    use serde_json::json;
    use std::sync::Mutex as StdMutex;

    #[derive(Clone, Default)]
    struct Record {
        specs: Vec<ExecSpec>,
        execs: Vec<Vec<String>>,
        attaches: Vec<AttachedSpec>,
        copies: Vec<(String, Vec<u8>)>,
        destroys: usize,
    }

    /// A scripted stand-in for a container: records everything, answers
    /// `initialize` when asked, and never touches a daemon.
    #[derive(Default)]
    struct StubSandbox {
        record: StdMutex<Record>,
        spawn_fail: bool,
        exec_fail: bool,
        mkdir_exit: i32,
        copy_fail: bool,
        attach_fail: bool,
        destroy_fail: bool,
        answer_initialize: bool,
    }

    impl StubSandbox {
        fn recording(&self) -> std::sync::MutexGuard<'_, Record> {
            self.record.lock().unwrap()
        }

        /// Owned copy of the log, for assertions that happen before an await.
        fn snapshot(&self) -> Record {
            self.recording().clone()
        }

        fn with_answer() -> Self {
            Self {
                answer_initialize: true,
                ..Default::default()
            }
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for StubSandbox {
        async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            if self.spawn_fail {
                return Err(SandboxError::Spawn("no daemon".into()));
            }
            self.recording().specs.push(spec.clone());
            Ok(SandboxHandle { id: "sbx-1".into() })
        }

        async fn exec(
            &self,
            _handle: &SandboxHandle,
            cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            self.recording().execs.push(cmd.to_vec());
            if self.exec_fail {
                return Err(SandboxError::Exec("daemon went away".into()));
            }
            Ok(ExecOutput {
                exit_code: self.mkdir_exit,
                stdout: String::new(),
                stderr: if self.mkdir_exit == 0 {
                    String::new()
                } else {
                    "permission denied".into()
                },
            })
        }

        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            self.recording().destroys += 1;
            if self.destroy_fail {
                return Err(SandboxError::Exec("container already gone".into()));
            }
            Ok(())
        }

        async fn copy_in(
            &self,
            _handle: &SandboxHandle,
            target: &str,
            tar: Vec<u8>,
        ) -> Result<(), SandboxError> {
            if self.copy_fail {
                return Err(SandboxError::Exec("upload refused".into()));
            }
            self.recording().copies.push((target.to_string(), tar));
            Ok(())
        }

        async fn exec_attached(
            &self,
            _handle: &SandboxHandle,
            spec: &AttachedSpec,
        ) -> Result<AttachedExec, SandboxError> {
            if self.attach_fail {
                return Err(SandboxError::Exec("cannot attach".into()));
            }
            self.recording().attaches.push(spec.clone());
            let (client, server) = tokio::io::duplex(4096);
            let answer = self.answer_initialize;
            tokio::spawn(async move {
                let (reader, mut writer) = tokio::io::split(server);
                let mut lines = BufReader::new(reader).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Some(reply) = stub_reply(&line, answer) else {
                        continue;
                    };
                    let _ = writer.write_all(reply.as_bytes()).await;
                    let _ = writer.write_all(b"\n").await;
                    let _ = writer.flush().await;
                }
            });
            let (reader, writer) = tokio::io::split(client);
            Ok(AttachedExec {
                exec_id: "exec-1".into(),
                stdin: Box::pin(writer),
                output: line_stream(reader),
            })
        }
    }

    /// The stub's scripted answer.
    ///
    /// Pulled out of the spawned task so every branch (silent, malformed,
    /// unrelated method, `initialize`) is a synchronous, deterministic test
    /// instead of a race with the handshake timeout.
    fn stub_reply(line: &str, answer: bool) -> Option<String> {
        if !answer {
            return None;
        }
        let frame: serde_json::Value = serde_json::from_str(line).ok()?;
        if frame["method"] != crate::rpc::METHOD_INITIALIZE {
            return None;
        }
        Some(format!(
            r#"{{"jsonrpc":"2.0","id":{},"result":{{"ok":true}}}}"#,
            frame["id"]
        ))
    }

    fn sandbox_cfg(stub: Arc<StubSandbox>, root: &Path) -> DshConfig {
        let mut cfg = DshConfig::for_session(root, "sess-1", "deepseek-v4");
        let sandbox = DshSandbox {
            sandbox: stub,
            image: "aria-dsh-sandbox:latest".into(),
            mount: SandboxMount::Volume,
            root: DEFAULT_ROOT.into(),
            bin: DEFAULT_BIN.into(),
            volume_prefix: "aria-dsh".into(),
            resources: SandboxResourceLimits {
                cpus: 1.0,
                memory_bytes: 1024 * 1024 * 1024,
                pids_limit: 512,
            },
            hardening: SandboxHardening::locked_down().with_user("10001:10001"),
            network: Some("aria-egress".into()),
            labels: vec![("aria.tenant_id".into(), "tenant-a".into())],
        };
        cfg = cfg.with_sandbox(sandbox);
        cfg
    }

    #[test]
    fn mount_mode_parses_case_insensitively_and_rejects_typos() {
        assert_eq!(SandboxMount::parse("Volume"), Some(SandboxMount::Volume));
        assert_eq!(SandboxMount::parse(" bind "), Some(SandboxMount::Bind));
        assert_eq!(SandboxMount::parse("tmpfs"), None);
    }

    #[test]
    fn the_stub_only_answers_initialize_when_asked() {
        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": crate::rpc::METHOD_INITIALIZE,
        })
        .to_string();
        let reply = stub_reply(&initialize, true).expect("answered");
        assert!(reply.contains("\"id\":7"), "{reply}");
        // Silent, malformed, and unrelated-method frames are all "keep reading".
        assert!(stub_reply(&initialize, false).is_none());
        assert!(stub_reply("not json", true).is_none());
        assert!(stub_reply(r#"{"id":1,"method":"session/prompt"}"#, true).is_none());
    }

    #[test]
    fn the_debug_view_names_the_image_and_network_only() {
        let temp = tempfile::tempdir().unwrap();
        let sandbox = sandbox_cfg(Arc::new(StubSandbox::default()), temp.path())
            .sandbox
            .clone()
            .unwrap();
        let shown = format!("{sandbox:?}");
        assert!(shown.contains("aria-dsh-sandbox:latest"), "{shown}");
        assert!(shown.contains("aria-egress"), "{shown}");
    }

    #[test]
    fn a_session_root_without_a_leaf_falls_back_to_the_default_tenant() {
        let temp = tempfile::tempdir().unwrap();
        let mut cfg = sandbox_cfg(Arc::new(StubSandbox::default()), temp.path());
        // `/` has no `file_name`, so the tenant component cannot be derived.
        cfg.session_root = PathBuf::from("/");
        let sandbox = cfg.sandbox.clone().unwrap();
        let mount = sandbox.mount_for(&cfg).unwrap();
        assert!(
            mount.source.starts_with("aria-dsh-default-"),
            "{}",
            mount.source
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_catalog_produces_no_archive() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        std::fs::create_dir_all(&catalog).unwrap();
        // A dangling symlink makes `append_dir_all` fail instead of packing.
        std::os::unix::fs::symlink("/definitely/missing", catalog.join("broken")).unwrap();
        assert!(skill_catalog_tar(&catalog).is_none());
    }

    #[test]
    fn volume_names_are_sanitised_and_tenant_scoped() {
        assert_eq!(
            volume_name("aria-dsh", "tenant-a", "sess_1"),
            "aria-dsh-tenant-a-sess_1"
        );
        assert_eq!(
            volume_name("aria-dsh", "tenant/a:b", "s"),
            "aria-dsh-tenant-a-b-s"
        );
    }

    #[test]
    fn the_session_mount_is_deterministic_and_container_rooted() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox::default());
        let cfg = sandbox_cfg(stub, temp.path());
        let sandbox = cfg.sandbox.clone().unwrap();
        let mount = sandbox.mount_for(&cfg).unwrap();
        assert_eq!(mount.kind, MountKind::Volume);
        assert_eq!(mount.target, DEFAULT_ROOT);
        assert!(mount.source.starts_with("aria-dsh-"), "{}", mount.source);
        // Same session, same volume: a resumed runtime finds its state.
        assert_eq!(mount.source, sandbox.mount_for(&cfg).unwrap().source);
    }

    #[test]
    fn bind_mode_creates_the_host_directory() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox::default());
        let mut cfg = sandbox_cfg(stub, temp.path());
        cfg.sandbox.as_mut().unwrap().mount = SandboxMount::Bind;
        let sandbox = cfg.sandbox.clone().unwrap();
        let mount = sandbox.mount_for(&cfg).unwrap();
        assert_eq!(mount.kind, MountKind::Bind);
        assert!(Path::new(&mount.source).is_dir(), "{}", mount.source);
        assert!(mount.source.contains("sess-1"));
    }

    #[test]
    fn bind_mode_reports_an_unusable_host_directory() {
        let temp = tempfile::tempdir().unwrap();
        // A file where the session root should be: every path under it fails to
        // become a directory, exactly like an unwritable host.
        let blocker = temp.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let stub = Arc::new(StubSandbox::default());
        let mut cfg = sandbox_cfg(stub, &blocker);
        cfg.sandbox.as_mut().unwrap().mount = SandboxMount::Bind;
        let sandbox = cfg.sandbox.clone().unwrap();
        let err = sandbox.mount_for(&cfg).expect_err("expected an error");
        assert!(err.to_string().contains("cannot create"), "{err}");
    }

    #[tokio::test]
    async fn connect_runs_the_harness_in_a_hardened_container() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox::with_answer());
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let runtime = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .unwrap();

        let record = stub.snapshot();
        let spec = record.specs.first().expect("spawned");
        assert_eq!(spec.image.as_deref(), Some("aria-dsh-sandbox:latest"));
        assert_eq!(spec.network.as_deref(), Some("aria-egress"));
        assert_eq!(spec.mounts.len(), 1);
        assert_eq!(spec.mounts[0].target, DEFAULT_ROOT);
        assert_eq!(spec.hardening.user.as_deref(), Some("10001:10001"));
        assert_eq!(spec.hardening.cap_drop, vec!["ALL".to_string()]);
        assert_eq!(
            spec.resources.map(|r| r.memory_bytes),
            Some(1024 * 1024 * 1024)
        );
        // The child env is the container's: DSH_HOME points at the volume and
        // PATH is the image's, never the host's.
        let env: std::collections::HashMap<_, _> = spec.env.iter().cloned().collect();
        assert_eq!(
            env.get("DSH_HOME").map(String::as_str),
            Some("/aria/session/dsh")
        );
        assert_eq!(env.get("PATH").map(String::as_str), Some(CONTAINER_PATH));
        // Workspace prepared inside the container, then the harness attached.
        assert!(record
            .execs
            .iter()
            .any(|cmd| cmd[0] == "mkdir" && cmd.contains(&"/aria/session/cwd".to_string())));
        let attach = record.attaches.first().expect("attached");
        assert_eq!(attach.command, vec![DEFAULT_BIN, "--profile", "sdk"]);
        assert_eq!(attach.workdir.as_deref(), Some("/aria/session/cwd"));
        drop(record);

        // Shutdown tears the container down.
        runtime.shutdown().await.unwrap();
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn a_failed_spawn_is_reported_without_a_container_to_clean() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox {
            spawn_fail: true,
            ..Default::default()
        });
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("spawn"), "{err}");
        assert_eq!(stub.recording().destroys, 0);
    }

    #[tokio::test]
    async fn an_unprepared_workspace_destroys_the_container() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox {
            mkdir_exit: 1,
            ..Default::default()
        });
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("cannot prepare"), "{err}");
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn a_refused_skill_upload_destroys_the_container() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("skills");
        std::fs::create_dir_all(catalog.join("pdf")).unwrap();
        std::fs::write(catalog.join("pdf/SKILL.md"), b"# pdf").unwrap();
        let stub = Arc::new(StubSandbox {
            copy_fail: true,
            ..Default::default()
        });
        let mut cfg = sandbox_cfg(stub.clone(), temp.path());
        cfg.skills = Some(catalog);
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("skill catalog"), "{err}");
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn an_unattachable_harness_destroys_the_container() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox {
            attach_fail: true,
            ..Default::default()
        });
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(
            err.to_string().contains("cannot start the harness"),
            "{err}"
        );
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn a_silent_harness_times_out_and_cleans_up() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox::default());
        let mut cfg = sandbox_cfg(stub.clone(), temp.path());
        cfg.init_timeout = Duration::from_millis(20);
        cfg.request_timeout = Duration::from_millis(20);
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("initialize"), "{err}");
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn the_skill_catalog_is_uploaded_as_a_tar() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = temp.path().join("catalog");
        std::fs::create_dir_all(catalog.join("pdf")).unwrap();
        std::fs::write(catalog.join("pdf/SKILL.md"), b"# pdf").unwrap();
        let stub = Arc::new(StubSandbox::with_answer());
        let mut cfg = sandbox_cfg(stub.clone(), temp.path());
        cfg.skills = Some(catalog);
        let runtime = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .unwrap();
        let record = stub.snapshot();
        let (target, tar) = record.copies.first().expect("uploaded");
        assert_eq!(target, "/aria/session/cwd/.aria");
        assert!(
            tar.windows(7).any(|w| w == b"skills/"),
            "tar has no skills/"
        );
        drop(record);
        runtime.shutdown().await.unwrap();
    }

    #[test]
    fn a_missing_catalog_produces_no_archive() {
        let temp = tempfile::tempdir().unwrap();
        assert!(skill_catalog_tar(&temp.path().join("absent")).is_none());
    }

    #[tokio::test]
    async fn the_transport_speaks_newline_delimited_frames() {
        let (client, server) = tokio::io::duplex(1024);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (server_reader, _server_writer) = tokio::io::split(server);
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            None,
            1024,
            Duration::from_millis(200),
        );
        transport.send_line("hello").await.unwrap();
        let mut lines = BufReader::new(server_reader).lines();
        assert_eq!(lines.next_line().await.unwrap().as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn partial_chunks_are_reassembled_into_lines() {
        let (client, server) = tokio::io::duplex(1024);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (_server_reader, mut server_writer) = tokio::io::split(server);
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            None,
            1024,
            Duration::from_millis(200),
        );
        // A frame split across two writes must be joined by the decoder.
        server_writer.write_all(b"par").await.unwrap();
        server_writer.flush().await.unwrap();
        server_writer.write_all(b"tial\n").await.unwrap();
        server_writer.flush().await.unwrap();
        assert_eq!(
            transport.recv_line().await.unwrap().as_deref(),
            Some("partial")
        );
    }

    #[tokio::test]
    async fn an_idle_transport_times_out_instead_of_hanging() {
        let (client, server) = tokio::io::duplex(64);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (_server_reader, _server_writer) = tokio::io::split(server);
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            None,
            1024,
            Duration::from_millis(10),
        );
        let err = transport.recv_line().await.unwrap_err();
        assert!(err.to_string().contains("no output"), "{err}");
    }

    #[tokio::test]
    async fn an_oversized_frame_is_a_protocol_error() {
        let (client, server) = tokio::io::duplex(1024);
        let (client_reader, client_writer) = tokio::io::split(client);
        let (_server_reader, mut server_writer) = tokio::io::split(server);
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            None,
            8,
            Duration::from_millis(500),
        );
        server_writer.write_all(b"0123456789").await.unwrap();
        server_writer.flush().await.unwrap();
        let err = transport.recv_line().await.unwrap_err();
        assert!(err.to_string().contains("exceeds 8 bytes"), "{err}");
    }

    #[tokio::test]
    async fn a_closed_pipe_ends_the_stream() {
        let (client, server) = tokio::io::duplex(64);
        let (client_reader, client_writer) = tokio::io::split(client);
        // Both halves must go: `split` keeps the duplex open until the last one.
        drop(tokio::io::split::<tokio::io::DuplexStream>(server));
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            None,
            1024,
            Duration::from_millis(200),
        );
        assert!(transport.recv_line().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn closing_is_idempotent_and_destroys_once() {
        let (client, server) = tokio::io::duplex(64);
        let (client_reader, client_writer) = tokio::io::split(client);
        let _server = tokio::io::split(server);
        let stub = Arc::new(StubSandbox::default());
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            Some(Teardown {
                sandbox: stub.clone(),
                handle: SandboxHandle { id: "sbx".into() },
            }),
            1024,
            Duration::from_millis(50),
        );
        transport.close().await.unwrap();
        transport.close().await.unwrap();
        assert_eq!(stub.recording().destroys, 1);
    }

    /// A stream of frames a scripted container produced (test only).
    fn feed(chunks: Vec<Result<LogChunk, SandboxError>>) -> agent_sandbox::AttachedOutput {
        Box::pin(async_stream::stream! {
            for chunk in chunks {
                yield chunk;
            }
        })
    }

    /// A writer that fails on the Nth write or on flush.
    struct BadWriter {
        /// Writes that succeed before the failure (`usize::MAX` = never).
        writes_ok: usize,
        written: usize,
        fail_flush: bool,
    }

    impl BadWriter {
        fn failing_on_write(n: usize) -> Self {
            Self {
                writes_ok: n,
                written: 0,
                fail_flush: false,
            }
        }

        fn failing_on_flush() -> Self {
            Self {
                writes_ok: usize::MAX,
                written: 0,
                fail_flush: true,
            }
        }

        fn healthy() -> Self {
            Self {
                writes_ok: usize::MAX,
                written: 0,
                fail_flush: false,
            }
        }
    }

    impl tokio::io::AsyncWrite for BadWriter {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            if self.written >= self.writes_ok {
                return std::task::Poll::Ready(Err(std::io::Error::other("write refused")));
            }
            self.written += 1;
            std::task::Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            if self.fail_flush {
                return std::task::Poll::Ready(Err(std::io::Error::other("flush refused")));
            }
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    fn transport_with(output: agent_sandbox::AttachedOutput) -> SandboxTransport {
        SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(tokio::io::sink()),
                output,
            },
            None,
            1024,
            Duration::from_millis(50),
        )
    }

    #[tokio::test]
    async fn stderr_is_skipped_and_a_read_error_is_not() {
        let transport = transport_with(feed(vec![
            Ok(LogChunk::Stderr("warning\n".into())),
            Ok(LogChunk::Stdout("answer\n".into())),
        ]));
        assert_eq!(
            transport.recv_line().await.unwrap().as_deref(),
            Some("answer")
        );

        let failing = transport_with(feed(vec![Err(SandboxError::Exec("pipe closed".into()))]));
        let err = failing.recv_line().await.expect_err("expected an error");
        assert!(err.to_string().contains("transport read failed"), "{err}");
    }

    fn transport_with_writer(writer: BadWriter) -> SandboxTransport {
        SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(writer),
                output: Box::pin(futures::stream::empty()),
            },
            None,
            1024,
            Duration::from_millis(50),
        )
    }

    #[tokio::test]
    async fn a_write_or_flush_failure_is_reported() {
        // `send_line` writes the payload, writes the newline, then flushes: each
        // of the three steps reports its own failure.
        let first = transport_with_writer(BadWriter::failing_on_write(0))
            .send_line("{}")
            .await
            .expect_err("expected an error");
        assert!(first.to_string().contains("write failed"), "{first}");

        let newline = transport_with_writer(BadWriter::failing_on_write(1))
            .send_line("{}")
            .await
            .expect_err("expected an error");
        assert!(newline.to_string().contains("write failed"), "{newline}");

        let flush = transport_with_writer(BadWriter::failing_on_flush())
            .send_line("{}")
            .await
            .expect_err("expected an error");
        assert!(flush.to_string().contains("flush failed"), "{flush}");

        transport_with_writer(BadWriter::healthy())
            .send_line("{}")
            .await
            .unwrap();

        // Closing the write half must not fail either.
        let mut writer = BadWriter::healthy();
        writer.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_cleanup_is_logged_but_the_prepare_error_surfaces() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox {
            mkdir_exit: 1,
            destroy_fail: true,
            ..Default::default()
        });
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("cannot prepare"), "{err}");
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn a_workspace_exec_error_destroys_the_container() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox {
            exec_fail: true,
            ..Default::default()
        });
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let err = crate::runtime::DshRuntime::connect(cfg, crate::runtime::no_bridge())
            .await
            .err()
            .expect("expected an error");
        assert!(err.to_string().contains("workspace"), "{err}");
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn a_container_that_is_already_gone_does_not_fail_close() {
        let (client, server) = tokio::io::duplex(64);
        let (client_reader, client_writer) = tokio::io::split(client);
        let _server = tokio::io::split(server);
        let stub = Arc::new(StubSandbox {
            destroy_fail: true,
            ..Default::default()
        });
        let transport = SandboxTransport::new(
            AttachedExec {
                exec_id: "e".into(),
                stdin: Box::pin(client_writer),
                output: byte_stream(client_reader),
            },
            Some(Teardown {
                sandbox: stub.clone(),
                handle: SandboxHandle { id: "sbx".into() },
            }),
            1024,
            Duration::from_millis(50),
        );
        // Close is idempotent by contract: a teardown that is already gone is
        // logged, not surfaced, and the teardown is still consumed.
        transport.close().await.unwrap();
        transport.close().await.unwrap();
        assert_eq!(stub.recording().destroys, 1);
    }

    #[tokio::test]
    async fn the_pool_reuses_one_session_config_without_extra_nesting() {
        let temp = tempfile::tempdir().unwrap();
        let stub = Arc::new(StubSandbox::with_answer());
        let cfg = sandbox_cfg(stub.clone(), temp.path());
        let pool = DshPool::new(cfg, 4, Duration::from_secs(60), crate::runtime::no_bridge());
        let runtime = pool.acquire("sess-1").await.unwrap();
        let _ = runtime;
        // The container path is stable: the session root is not re-appended.
        let record = stub.recording();
        assert!(record
            .execs
            .iter()
            .any(|cmd| cmd.iter().any(|part| part == "/aria/session/cwd")));
        assert!(record
            .execs
            .iter()
            .all(|cmd| !cmd.iter().any(|part| part.contains(".aria/dsh/.aria/dsh"))));
    }
}
