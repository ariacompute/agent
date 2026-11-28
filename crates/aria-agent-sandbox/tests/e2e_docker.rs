//! Real-container check for the primitives the sandboxed dsh runtime depends on.
//!
//! Gated, never part of the default run:
//!
//! ```text
//! ARIA_E2E_DOCKER=1 cargo test -p aria-agent-sandbox --test e2e_docker -- --nocapture
//! ```
//!
//! It needs a reachable Docker daemon and the `alpine:latest` image (the same
//! precondition as the repository's other Docker e2e suite). Without them the
//! test prints why and returns — it never fabricates a pass.
//!
//! What only a real daemon can prove: `exec_attached` really is **duplex** (a
//! scripted server can fake it), the raw-stream framing survives a live
//! container, and `copy_in` unpacks where the caller asked.

use std::time::Duration;

use agent_sandbox::{AttachedSpec, ExecSpec, MountSpec, Sandbox, SandboxHardening};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const IMAGE: &str = "alpine:latest";

fn enabled() -> bool {
    std::env::var("ARIA_E2E_DOCKER").ok().as_deref() == Some("1")
}

fn spec() -> ExecSpec {
    let mut spec = ExecSpec::command(vec!["true".into()]);
    spec.image = Some(IMAGE.to_string());
    spec.workdir = Some("/work".into());
    spec.hardening = SandboxHardening::default();
    spec
}

#[tokio::test]
async fn a_session_speaks_over_attached_stdio_and_accepts_files() {
    if !enabled() {
        eprintln!("skip: set ARIA_E2E_DOCKER=1 to run the real-container sandbox test");
        return;
    }
    let sandbox = agent_sandbox::DockerSandbox::new();
    if !sandbox.has_image(IMAGE).await.unwrap_or(false) {
        eprintln!("skip: image {IMAGE} is not cached locally");
        return;
    }
    let handle = sandbox.spawn(&spec()).await.expect("spawn");

    // 1) Duplex: a shell that echoes one line back, then exits.
    let mut attached = sandbox
        .exec_attached(
            &handle,
            &AttachedSpec::command(vec![
                "sh".into(),
                "-c".into(),
                "read line; echo got:$line".into(),
            ]),
        )
        .await
        .expect("attach");
    attached
        .stdin
        .write_all(b"hello-from-the-host\n")
        .await
        .expect("stdin");
    attached.stdin.flush().await.expect("flush");
    let mut lines = BufReader::new(StreamReader::new(attached.output)).lines();
    let reply = tokio::time::timeout(Duration::from_secs(10), lines.next_line())
        .await
        .expect("the echo must arrive within 10s")
        .expect("stream")
        .expect("line");
    assert_eq!(reply.trim(), "got:hello-from-the-host");

    // 2) copy_in: the archive lands under the target directory.
    let archive = tar_with_file("seed.txt", b"seeded\n");
    sandbox
        .copy_in(&handle, "/work", archive)
        .await
        .expect("copy_in");
    let out = sandbox
        .exec(&handle, &["cat".into(), "/work/seed.txt".into()])
        .await
        .expect("exec cat");
    assert_eq!(out.exit_code, 0, "stderr={}", out.stderr);
    assert_eq!(out.stdout, "seeded\n");

    sandbox.destroy(handle).await.expect("destroy");
}

/// A one-entry ustar archive, so the test does not depend on a shell pipeline.
fn tar_with_file(name: &str, body: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder
        .append_data(&mut header, name, std::io::Cursor::new(body))
        .expect("tar entry");
    builder.into_inner().expect("tar bytes")
}

/// `Stream<LogChunk>` → `AsyncRead`, so the test can share the line reader with
/// the rest of the file instead of hand-rolling a buffer.
struct StreamReader {
    inner: std::pin::Pin<
        Box<
            dyn futures::Stream<Item = Result<agent_sandbox::LogChunk, agent_sandbox::SandboxError>>
                + Send,
        >,
    >,
    buffer: Vec<u8>,
}

impl StreamReader {
    fn new(
        inner: std::pin::Pin<
            Box<
                dyn futures::Stream<
                        Item = Result<agent_sandbox::LogChunk, agent_sandbox::SandboxError>,
                    > + Send,
            >,
        >,
    ) -> Self {
        Self {
            inner,
            buffer: Vec::new(),
        }
    }
}

impl tokio::io::AsyncRead for StreamReader {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::task::Poll;
        loop {
            if !self.buffer.is_empty() {
                let take = self.buffer.len().min(buf.remaining());
                buf.put_slice(&self.buffer[..take]);
                self.buffer.drain(..take);
                return Poll::Ready(Ok(()));
            }
            match self.inner.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(agent_sandbox::LogChunk::Stdout(chunk)))) => {
                    self.buffer.extend_from_slice(chunk.as_bytes());
                }
                Poll::Ready(Some(Ok(agent_sandbox::LogChunk::Stderr(_)))) => continue,
                Poll::Ready(Some(Err(e))) => {
                    return Poll::Ready(Err(std::io::Error::other(e.to_string())))
                }
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

#[tokio::test]
async fn a_volume_mount_survives_a_container_rebuild() {
    if !enabled() {
        eprintln!("skip: set ARIA_E2E_DOCKER=1 to run the real-container sandbox test");
        return;
    }
    let sandbox = agent_sandbox::DockerSandbox::new();
    if !sandbox.has_image(IMAGE).await.unwrap_or(false) {
        eprintln!("skip: image {IMAGE} is not cached locally");
        return;
    }
    let volume = format!("aria-e2e-{}", uuid_like());
    let mut first = spec();
    first.mounts = vec![MountSpec::volume(volume.clone(), "/sessions")];

    // The volume outlives both containers and is global state: clean it up even
    // when an assertion fires, so a red run cannot leave anything behind.
    let outcome = a_rebuild_sees_the_same_volume(&sandbox, &first).await;
    drop_volume(&volume).await;
    if let Err(message) = outcome {
        panic!("{message}");
    }
}

/// Reports instead of panicking so the caller removes the volume first.
async fn a_rebuild_sees_the_same_volume(
    sandbox: &agent_sandbox::DockerSandbox,
    spec: &ExecSpec,
) -> Result<(), String> {
    let handle = sandbox
        .spawn(spec)
        .await
        .map_err(|e| format!("spawn with volume: {e}"))?;
    let written = sandbox
        .exec(
            &handle,
            &[
                "sh".into(),
                "-c".into(),
                "mkdir -p /sessions && echo kept > /sessions/marker".into(),
            ],
        )
        .await
        .map_err(|e| format!("write marker: {e}"));
    sandbox
        .destroy(handle)
        .await
        .map_err(|e| format!("destroy first: {e}"))?;
    let written = written?;
    if written.exit_code != 0 {
        return Err(format!(
            "write marker exited {}: {}",
            written.exit_code, written.stderr
        ));
    }

    // A brand-new container on the same volume sees the file: this is what makes
    // an evicted dsh session resumable.
    let second = sandbox
        .spawn(spec)
        .await
        .map_err(|e| format!("respawn with the same volume: {e}"))?;
    let seen = sandbox
        .exec(&second, &["cat".into(), "/sessions/marker".into()])
        .await
        .map_err(|e| format!("read marker: {e}"));
    sandbox
        .destroy(second)
        .await
        .map_err(|e| format!("destroy second: {e}"))?;
    let seen = seen?;
    if seen.stdout != "kept\n" {
        return Err(format!(
            "the rebuild read {:?} instead of the marker",
            seen.stdout
        ));
    }
    Ok(())
}

/// Best effort, never fails the test: removing an already-gone volume is fine.
async fn drop_volume(name: &str) {
    let Ok(docker) = bollard::Docker::connect_with_local_defaults() else {
        return;
    };
    if let Err(error) = docker
        .remove_volume(name, None::<bollard::volume::RemoveVolumeOptions>)
        .await
    {
        eprintln!("warning: could not remove volume {name}: {error}");
    }
}

fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}")
}
