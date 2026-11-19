//! Process-level tests against a scripted fake `dsh` child.
//!
//! These exercise the real `StdioTransport` (spawn, line framing, EOF, kill) and
//! `DshRuntime::connect`/`prompt` without DeepSeek credentials: the "runtime" is
//! a shell script that answers JSON-RPC. Unix-only; on other hosts the test is
//! compiled out rather than skipped silently at run time.

#![cfg(unix)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use aria_agent_dsh::{
    no_bridge, DshConfig, DshRuntime, HarnessRuntime, RpcTransport, METHOD_INITIALIZE,
    METHOD_SESSION_PROMPT,
};
use serde_json::Value;

/// Write an executable fake runtime that echoes each request's id.
fn fake_runtime(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-dsh");
    let script = r##"#!/bin/sh
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *session/prompt*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"queued":true}}\n' "$id"
      printf '{"jsonrpc":"2.0","method":"turn/start","params":{}}\n'
      printf '{"jsonrpc":"2.0","method":"assistant/message","params":{"content":"pong"}}\n'
      printf '{"jsonrpc":"2.0","method":"turn/end","params":{"reason":{"kind":"completed"}}}\n'
      ;;
    *)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
      ;;
  esac
done
"##;
    let mut file = std::fs::File::create(&path).expect("create script");
    file.write_all(script.as_bytes()).expect("write script");
    drop(file);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

#[tokio::test]
async fn the_real_transport_completes_a_handshake_and_a_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_runtime(dir.path());
    let cfg = DshConfig::for_session(dir.path(), "s1", "fake-model")
        .with_bin(bin.to_string_lossy().to_string());
    let runtime = DshRuntime::connect(cfg, no_bridge())
        .await
        .expect("handshake succeeds");

    let mut rx = runtime.prompt("s1", "ping").await.expect("prompt queued");
    let mut methods = Vec::new();
    while let Some(frame) = rx.recv().await {
        methods.push(frame["method"].as_str().unwrap_or("").to_string());
        if methods.len() == 3 {
            break;
        }
    }
    assert_eq!(methods, vec!["turn/start", "assistant/message", "turn/end"]);
    runtime.shutdown().await.expect("shutdown");

    // The method names are the ones we document.
    assert_eq!(METHOD_INITIALIZE, "initialize");
    assert_eq!(METHOD_SESSION_PROMPT, "session/prompt");
}

#[tokio::test]
async fn a_missing_binary_is_an_actionable_config_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cfg = DshConfig::for_session(dir.path(), "s1", "fake-model").with_bin(
        dir.path()
            .join("does-not-exist")
            .to_string_lossy()
            .to_string(),
    );
    let err = DshRuntime::connect(cfg, no_bridge())
        .await
        .err()
        .expect("must fail closed");
    assert!(
        err.to_string().contains("cannot start DeepSeek Harness"),
        "{err}"
    );
    assert!(
        err.to_string().contains("ARIA_AGENT_HARNESS=native"),
        "{err}"
    );
}

#[tokio::test]
async fn a_runtime_that_exits_during_the_handshake_is_reported() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = dir.path().join("dying-dsh");
    std::fs::write(&bin, "#!/bin/sh\nexit 3\n").expect("write");
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod");
    }
    let mut cfg = DshConfig::for_session(dir.path(), "s1", "fake-model")
        .with_bin(bin.to_string_lossy().to_string());
    cfg.init_timeout = Duration::from_secs(2);
    let err = DshRuntime::connect(cfg, no_bridge())
        .await
        .err()
        .expect("must not hang");
    // Depending on timing the transport either sees EOF or a broken pipe; both
    // mean "runtime died during the handshake" and must be surfaced.
    assert!(
        err.to_string().contains("exited") || err.to_string().contains("write failed"),
        "{err}"
    );
    let _ = Arc::new(Value::Null);
}

/// Write an executable script with the given body.
fn script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod");
    }
    path
}

#[tokio::test]
async fn an_oversized_frame_is_a_protocol_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = script(
        dir.path(),
        "loud-dsh",
        "#!/bin/sh\nprintf '%0.sx' $(seq 1 200)\nprintf '\\n'\nsleep 5\n",
    );
    let transport = aria_agent_dsh::StdioTransport::spawn(
        bin.to_string_lossy().as_ref(),
        &[],
        &[],
        dir.path(),
        64,
    )
    .expect("spawn");
    let err = transport.recv_line().await.expect_err("frame too large");
    assert!(err.to_string().contains("exceeds"), "{err}");
    transport.close().await.expect("close");
}

#[tokio::test]
async fn closing_kills_a_wedged_runtime_and_the_handshake_times_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = script(
        dir.path(),
        "wedged-dsh",
        "#!/bin/sh\nwhile true; do sleep 1; done\n",
    );
    let mut cfg = DshConfig::for_session(dir.path(), "s1", "fake-model")
        .with_bin(bin.to_string_lossy().to_string());
    cfg.init_timeout = Duration::from_millis(100);
    let err = DshRuntime::connect(cfg, no_bridge())
        .await
        .err()
        .expect("handshake must time out");
    assert!(err.to_string().contains("timed out"), "{err}");

    let transport = aria_agent_dsh::StdioTransport::spawn(
        bin.to_string_lossy().as_ref(),
        &[],
        &[],
        dir.path(),
        1024,
    )
    .expect("spawn");
    transport.close().await.expect("kill");
    // Closing an already-exited child stays idempotent.
    transport.close().await.expect("kill again");
}

#[tokio::test]
async fn the_pool_starts_and_reuses_a_real_runtime() {
    let dir = tempfile::tempdir().expect("tempdir");
    let bin = fake_runtime(dir.path());
    let cfg = DshConfig::for_session(dir.path(), "seed", "fake-model")
        .with_bin(bin.to_string_lossy().to_string());
    let pool = aria_agent_dsh::DshPool::new(cfg, 1, Duration::ZERO, no_bridge());
    let runtime = pool.acquire("real-s1").await.expect("runtime");
    let again = pool.acquire("real-s1").await.expect("reused");
    assert!(Arc::ptr_eq(&runtime, &again));
    assert_eq!(pool.len().await, 1);
    let mut rx = runtime.prompt("real-s1", "hi").await.expect("prompt");
    let first = rx.recv().await.expect("notification");
    assert_eq!(first["method"], "turn/start");
    assert_eq!(pool.sweep().await, 1);
    assert!(pool.is_empty().await);
}
