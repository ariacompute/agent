//! Live check: **a real dsh runtime inside a real container**.
//!
//! Gated, never part of the default run:
//!
//! ```text
//! just sandbox-image
//! ARIA_DSH_SANDBOX_E2E=1 cargo test -p aria-agent-dsh --test sandbox_live -- --nocapture
//! ```
//!
//! No model call and no gateway key: the `initialize` handshake is local, so this
//! proves the parts a scripted stub cannot — the image exists, the session
//! directory mounts at the configured root, the workspace is prepared, `dsh`
//! starts as an attached process, and the JSON-RPC handshake completes over the
//! container's stdio. It mounts a **bind** source (a temp directory it removes
//! afterwards) rather than a named volume, so a live run leaves nothing behind on
//! the daemon; the named-volume variant differs only in the mount source, which
//! `sandbox.rs` pins by name. A real turn is covered by `e2e_live.rs` (host) and
//! by a deployment smoke test.
//!
//! A missing image or an unreachable daemon is a loud skip, never a pass.

#![cfg(unix)]

use std::sync::Arc;
use std::time::Duration;

use agent_sandbox::{Sandbox, SandboxHardening, SandboxResourceLimits};
use aria_agent_dsh::{no_bridge, DshConfig, DshRuntime, DshSandbox, HarnessRuntime, SandboxMount};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// `uid:gid` of the account that owns the bind source (the test's own).
fn bind_owner(root: &std::path::Path) -> String {
    use std::os::unix::fs::MetadataExt;
    std::fs::create_dir_all(root).expect("session root");
    let meta = std::fs::metadata(root).expect("session root metadata");
    format!("{}:{}", meta.uid(), meta.gid())
}

fn skip_reason() -> Option<String> {
    if env("ARIA_DSH_SANDBOX_E2E").as_deref() != Some("1") {
        return Some("set ARIA_DSH_SANDBOX_E2E=1 to run the containerised dsh test".into());
    }
    if env("GATEWAY_BASE_URL").is_none() {
        // `initialize` does not call the model, but dsh still wants a provider
        // base; an empty one keeps the handshake offline.
        return Some("GATEWAY_BASE_URL is not set (the handshake needs a provider route)".into());
    }
    None
}

#[tokio::test]
async fn a_containerised_dsh_completes_its_handshake() {
    if let Some(reason) = skip_reason() {
        eprintln!("skip: {reason}");
        return;
    }
    let image = env("DSH_SANDBOX_IMAGE").unwrap_or_else(|| "aria-dsh-sandbox:latest".to_string());
    let backend: Arc<dyn Sandbox> = Arc::new(agent_sandbox::DockerSandbox::new());
    if !backend.has_image(&image).await.unwrap_or(false) {
        eprintln!("skip: image {image} is not cached locally (run `just sandbox-image`)");
        return;
    }

    let root = std::env::temp_dir().join(format!("aria-dsh-live-{}", std::process::id()));
    let mut cfg = DshConfig::for_session(&root, "live-session", "deepseek-v4");
    cfg.init_timeout = Duration::from_secs(60);
    cfg.request_timeout = Duration::from_secs(120);
    cfg.permission_mode = "workspace-write".to_string();
    cfg.extra_env = vec![
        (
            "DEEPSEEK_BASE_URL".to_string(),
            env("GATEWAY_BASE_URL").unwrap_or_default(),
        ),
        (
            "DEEPSEEK_API_KEY".to_string(),
            env("GATEWAY_API_KEY").unwrap_or_default(),
        ),
    ];
    let sandbox = DshSandbox {
        sandbox: backend,
        image,
        mount: SandboxMount::Bind,
        root: "/aria/session".to_string(),
        bin: env("DSH_SANDBOX_BIN").unwrap_or_else(|| "/usr/local/bin/dsh".to_string()),
        volume_prefix: "aria-dsh-e2e".to_string(),
        resources: SandboxResourceLimits {
            cpus: 1.0,
            memory_bytes: 1024 * 1024 * 1024,
            pids_limit: 512,
        },
        // Bind mode hands a *host* directory to the container, so the harness must
        // run as its owner; the image's default uid cannot write a directory the
        // test user created. This is the documented contract for `DSH_SANDBOX_MOUNT=bind`.
        hardening: SandboxHardening::locked_down().with_user(bind_owner(&root)),
        network: None,
        labels: Vec::new(),
    };

    let runtime = DshRuntime::connect(cfg.with_sandbox(sandbox), no_bridge())
        .await
        .expect("the sandboxed harness must complete `initialize`");
    runtime.shutdown().await.expect("shutdown");
    // The bind source belongs to this test: remove it (and the container is gone
    // because `shutdown` destroys the sandbox).
    let _ = std::fs::remove_dir_all(&root);
}
