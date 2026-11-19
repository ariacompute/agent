//! Network-enforcement egress proxy.
//!
//! Runs as its own container on the `internal` sandbox network plus the control
//! plane's network. It holds **no secrets and no policy**: every decision,
//! substitution and audit event goes back to the control plane, authenticated
//! with a shared token. An unidentifiable peer is refused.
//!
//! Environment:
//!   `EGRESS_PROXY_BIND`         listen address for proxied traffic (default `0.0.0.0:8080`)
//!   `EGRESS_PROXY_HEALTH_BIND`  health endpoint (default `0.0.0.0:8081`)
//!   `CONTROL_PLANE_URL`         required, e.g. `http://aria-agent-cloud:8080`
//!   `EGRESS_PROXY_TOKEN`        required, shared with the control plane
//!   `EGRESS_PROXY_PEER_TTL_SECS` peer identity cache TTL (default `10`)

mod audit;
mod factory;
mod remote;

use agent_core::seams::SeamError;
use agent_governance::LocalForwardProxy;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

const DEFAULT_BIND: &str = "0.0.0.0:8080";
const DEFAULT_HEALTH_BIND: &str = "0.0.0.0:8081";
const DEFAULT_PEER_TTL_SECS: u64 = 10;

#[tokio::main]
async fn main() {
    // Container healthcheck: connect to the health endpoint and mirror the
    // result in the exit code (the slim runtime image has no curl).
    if std::env::args().any(|arg| arg == "--healthcheck") {
        let bind = env_or("EGRESS_PROXY_HEALTH_BIND", DEFAULT_HEALTH_BIND);
        let addr = bind.replace("0.0.0.0", "127.0.0.1");
        match tokio::net::TcpStream::connect(&addr).await {
            Ok(_) => std::process::exit(0),
            Err(_) => std::process::exit(1),
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "aria_egress_proxy=info".into()),
        )
        .init();

    if let Err(e) = run().await {
        // Fail loudly: a proxy that starts without a control plane would either
        // deny everything (an outage) or allow everything (a bypass).
        tracing::error!(error = %e, "egress proxy cannot start");
        eprintln!("aria-egress-proxy: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), SeamError> {
    let control_url = env_required("CONTROL_PLANE_URL")?;
    let token = env_required("EGRESS_PROXY_TOKEN")?;
    let bind = env_or("EGRESS_PROXY_BIND", DEFAULT_BIND);
    let health_bind = env_or("EGRESS_PROXY_HEALTH_BIND", DEFAULT_HEALTH_BIND);
    let ttl = std::env::var("EGRESS_PROXY_PEER_TTL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_PEER_TTL_SECS);

    let control = Arc::new(remote::RemoteControl::new(control_url.clone(), token)?);
    let sink = audit::RemoteAudit::spawn(control.clone());
    let factory = Arc::new(factory::PeerGatewayFactory::new(
        control.clone(),
        sink.clone(),
        Duration::from_secs(ttl),
    ));

    tokio::spawn(health_server(health_bind.clone(), sink));
    let proxy = LocalForwardProxy::bind_with(&bind, factory).await?;
    tracing::info!(
        listen = %proxy.addr(),
        health = %health_bind,
        control_plane = %control_url,
        peer_ttl_secs = ttl,
        "egress proxy serving"
    );
    proxy.serve().await;
    Ok(())
}

fn env_required(key: &str) -> Result<String, SeamError> {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| {
            SeamError::Unavailable(format!(
                "{key} must be set; the proxy has no default and refuses to start without it"
            ))
        })
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// Minimal HTTP health endpoint: the container healthcheck only needs a 200.
/// The dropped-audit counter rides along so a full queue is visible from
/// outside the process.
async fn health_server(bind: String, sink: std::sync::Arc<audit::RemoteAudit>) {
    let listener = match TcpListener::bind(&bind).await {
        Ok(listener) => listener,
        Err(e) => {
            tracing::error!(bind = %bind, error = %e, "health endpoint cannot bind");
            return;
        }
    };
    loop {
        let Ok((mut stream, _)) = listener.accept().await else {
            continue;
        };
        let sink = sink.clone();
        tokio::spawn(async move {
            let mut scratch = [0u8; 512];
            let _ = stream.read(&mut scratch).await;
            let body = format!("ok\ndropped_audit={}\n", sink.dropped());
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
    }
}
