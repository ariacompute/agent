//! The single outbound interception point: policy → credential substitution →
//! audit.
//!
//! This is the "one boundary, three jobs" decision (ADR-0020). Two transport
//! shapes are supported:
//!
//! * **absolute-URI** (`GET http://host/path`) — plaintext HTTP, so headers can
//!   be rewritten by the credential broker;
//! * **CONNECT** (`CONNECT host:443`) — decided and audited, then blind-tunnelled.
//!   There is **no TLS interception**: no self-signed CA, no trust-store single
//!   point of failure, and in exchange a placeholder inside a TLS body cannot be
//!   substituted. That trade-off is stated in the startup log and the ADR rather
//!   than papered over.
//!
//! Everything is bounded: header reads are capped, upstream connects have a
//! timeout, and bodies are streamed rather than buffered.

use agent_core::seams::{
    AuditEvent, AuditKind, AuditSink, CredentialsSeam, EgressDecision, EgressSeam, SeamError,
    SeamResult, SubstitutionReport,
};
use serde_json::json;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Cap on the request head we are willing to buffer.
pub const MAX_HEADER_BYTES: usize = 64 * 1024;
/// Upstream connect timeout.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// One parsed HTTP request head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestHead {
    pub method: String,
    pub target: String,
    pub version: String,
    pub headers: Vec<(String, String)>,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// The policy + credential + audit trio, bound to one principal.
pub struct EgressGateway {
    pub egress: Arc<dyn EgressSeam>,
    pub credentials: Arc<dyn CredentialsSeam>,
    pub audit: Arc<dyn AuditSink>,
    pub principal_id: String,
    pub session_id: Option<String>,
}

impl std::fmt::Debug for EgressGateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EgressGateway")
            .field("principal_id", &self.principal_id)
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

impl EgressGateway {
    pub fn new(
        egress: Arc<dyn EgressSeam>,
        credentials: Arc<dyn CredentialsSeam>,
        audit: Arc<dyn AuditSink>,
        principal_id: impl Into<String>,
    ) -> Self {
        Self {
            egress,
            credentials,
            audit,
            principal_id: principal_id.into(),
            session_id: None,
        }
    }

    pub fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session_id = Some(session.into());
        self
    }

    /// Decide one destination and audit it (allowed or not — both matter).
    pub async fn authorize(&self, host: &str) -> SeamResult<EgressDecision> {
        let policy = self.egress.policy().await?;
        let decision = policy.decide(host);
        let mut event = AuditEvent::new(AuditKind::EgressDecision, &self.principal_id, host)
            .with_decision(decision.is_allowed())
            .with_policy_revision(policy.revision);
        if let Some(session) = &self.session_id {
            event = event.with_session(session.clone());
        }
        if let Some(reason) = decision.reason() {
            event = event.with_metadata(json!({ "reason": reason }));
        }
        // An audit failure must not turn a policy decision into an outage, but it
        // is visible in the logs.
        if let Err(e) = self.audit.record(event).await {
            tracing::warn!(error = %e, "audit sink rejected an egress decision");
        }
        Ok(decision)
    }

    /// Substitute credential stubs into the header set and audit the fact (never
    /// the values).
    pub async fn substitute(
        &self,
        host: &str,
        path: &str,
        headers: &mut Vec<(String, String)>,
    ) -> SeamResult<SubstitutionReport> {
        let report = self.credentials.substitute(host, path, headers).await?;
        if report.applied > 0 {
            // Path only: queries can carry material we must not persist.
            let audit_path = path.split('?').next().unwrap_or(path);
            let event = AuditEvent::new(AuditKind::CredentialSubstituted, &self.principal_id, host)
                .with_metadata(json!({
                    "stubs": report.stub_ids,
                    "count": report.applied,
                    "path": audit_path,
                }));
            if let Err(e) = self.audit.record(event).await {
                tracing::warn!(error = %e, "audit sink rejected a substitution");
            }
        }
        Ok(report)
    }
}

/// Resolves the identity of a connecting peer.
///
/// A single-tenant proxy has exactly one gateway; a shared proxy (the network
/// enforcement sidecar) derives the tenant from the peer address on every
/// connection. Making this a trait keeps one proxy implementation for both.
#[async_trait::async_trait]
pub trait GatewayFactory: Send + Sync {
    async fn for_peer(&self, peer: SocketAddr) -> SeamResult<Arc<EgressGateway>>;
}

/// Factory for the common single-tenant case.
pub struct SingleGateway(pub Arc<EgressGateway>);

#[async_trait::async_trait]
impl GatewayFactory for SingleGateway {
    async fn for_peer(&self, _peer: SocketAddr) -> SeamResult<Arc<EgressGateway>> {
        Ok(self.0.clone())
    }
}

/// The socket operations the proxy needs. A seam so a listener whose fd is
/// torn down (or whose address disappears) can be exercised in tests without a
/// real daemon; production always uses the `TcpListener` implementation.
#[async_trait::async_trait]
pub(crate) trait ProxyListener: Send + Sync {
    async fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)>;
    fn local_addr(&self) -> std::io::Result<SocketAddr>;
}

#[async_trait::async_trait]
impl ProxyListener for TcpListener {
    async fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
        tokio::net::TcpListener::accept(self).await
    }

    fn local_addr(&self) -> std::io::Result<SocketAddr> {
        tokio::net::TcpListener::local_addr(self)
    }
}

/// A local HTTP forward proxy bound to a loopback port.
pub struct LocalForwardProxy {
    listener: Box<dyn ProxyListener>,
    addr: SocketAddr,
    factory: Arc<dyn GatewayFactory>,
}

impl LocalForwardProxy {
    /// Bind a loopback listener. `bind` is usually `127.0.0.1:0` for tests or a
    /// fixed port in production.
    pub async fn bind(bind: &str, gateway: Arc<EgressGateway>) -> SeamResult<Self> {
        Self::bind_with(bind, Arc::new(SingleGateway(gateway))).await
    }

    /// Bind with per-connection identity resolution. The factory runs *before* a
    /// byte of the request is parsed; a factory error refuses the connection, so
    /// an unknown peer never reaches a decision.
    pub async fn bind_with(bind: &str, factory: Arc<dyn GatewayFactory>) -> SeamResult<Self> {
        let listener = TcpListener::bind(bind)
            .await
            .map_err(|e| SeamError::Io(format!("bind proxy {bind}: {e}")))?;
        Self::from_listener(Box::new(listener), factory)
    }

    fn from_listener(
        listener: Box<dyn ProxyListener>,
        factory: Arc<dyn GatewayFactory>,
    ) -> SeamResult<Self> {
        let addr = listener
            .local_addr()
            .map_err(|e| SeamError::Io(format!("proxy local_addr: {e}")))?;
        Ok(Self {
            listener,
            addr,
            factory,
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Value for `HTTP_PROXY` / `HTTPS_PROXY` inside the sandbox.
    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Accept forever, one task per connection.
    pub async fn serve(self) {
        loop {
            match self.listener.accept().await {
                Ok((stream, peer)) => {
                    let factory = self.factory.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_peer(stream, peer, factory).await {
                            tracing::debug!(error = %e, "proxy connection ended with an error");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!(error = %e, "proxy accept failed");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    }

    /// Accept and serve exactly one connection, then return. Test hook.
    pub async fn serve_one(&self) -> SeamResult<()> {
        let (stream, peer) = self
            .listener
            .accept()
            .await
            .map_err(|e| SeamError::Io(format!("proxy accept: {e}")))?;
        serve_peer(stream, peer, self.factory.clone()).await
    }
}

/// Resolve the peer, then hand the connection to the gateway.
///
/// An unresolvable peer is refused with `403` and never gets a decision — the
/// shared proxy must not fall back to a default tenant.
async fn serve_peer(
    mut client: TcpStream,
    peer: SocketAddr,
    factory: Arc<dyn GatewayFactory>,
) -> SeamResult<()> {
    match factory.for_peer(peer).await {
        Ok(gateway) => handle_connection(client, gateway).await,
        Err(e) => {
            let reason = format!("egress proxy could not identify peer {}: {e}", peer.ip());
            tracing::warn!(peer = %peer.ip(), error = %e, "refusing an unidentified egress peer");
            // Drain the request head before answering: closing with unread data
            // makes the kernel send RST, and the client would see a transport
            // error instead of the actionable 403.
            let mut sink = [0u8; 1024];
            let _ = tokio::time::timeout(Duration::from_millis(100), client.read(&mut sink)).await;
            send_bytes(&mut client, forbidden(&reason).as_bytes(), "write 403").await?;
            let _ = client.shutdown().await;
            Ok(())
        }
    }
}

async fn handle_connection(mut client: TcpStream, gateway: Arc<EgressGateway>) -> SeamResult<()> {
    let (head_raw, leftover) = read_head(&mut client).await?;
    let head = parse_head(&head_raw)?;

    if head.method.eq_ignore_ascii_case("CONNECT") {
        return tunnel(client, &head, leftover, gateway).await;
    }
    forward_http(client, &head, leftover, gateway).await
}

/// Write bytes to a peer.
///
/// A vanished client is a normal end of connection, but the failure must stay
/// visible instead of being swallowed: the caller decides what it means.
async fn send_bytes(stream: &mut TcpStream, bytes: &[u8], context: &str) -> SeamResult<()> {
    stream
        .write_all(bytes)
        .await
        .map_err(|e| SeamError::Io(format!("{context}: {e}")))?;
    Ok(())
}

/// Relay bytes in both directions until either side closes.
async fn relay(client: &mut TcpStream, upstream: &mut TcpStream, context: &str) -> SeamResult<()> {
    tokio::io::copy_bidirectional(client, upstream)
        .await
        .map_err(|e| SeamError::Io(format!("{context}: {e}")))?;
    Ok(())
}

async fn read_head(stream: &mut TcpStream) -> SeamResult<(String, Vec<u8>)> {
    let mut buffer = Vec::with_capacity(1024);
    loop {
        if let Some(pos) = find_head_end(&buffer) {
            let head = String::from_utf8_lossy(&buffer[..pos]).to_string();
            let leftover = buffer[pos..].to_vec();
            return Ok((head, leftover));
        }
        if buffer.len() >= MAX_HEADER_BYTES {
            return Err(SeamError::Invalid(format!(
                "request head exceeds {MAX_HEADER_BYTES} bytes"
            )));
        }
        let mut chunk = [0u8; 4096];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|e| SeamError::Io(format!("read request head: {e}")))?;
        if read == 0 {
            return Err(SeamError::Invalid("connection closed mid-head".into()));
        }
        buffer.extend_from_slice(&chunk[..read]);
    }
}

fn find_head_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

/// Parse a request line + headers. Malformed input is an `Invalid` error, never a
/// best-effort guess that could route traffic to the wrong host.
pub fn parse_head(raw: &str) -> SeamResult<RequestHead> {
    let mut lines = raw.split("\r\n").filter(|l| !l.is_empty());
    let request_line = lines
        .next()
        .ok_or_else(|| SeamError::Invalid("empty request".into()))?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| SeamError::Invalid(format!("malformed request line: {request_line}")))?;
    let target = parts
        .next()
        .ok_or_else(|| SeamError::Invalid(format!("malformed request line: {request_line}")))?;
    let version = parts.next().unwrap_or("HTTP/1.1");
    let mut headers = Vec::new();
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            return Err(SeamError::Invalid(format!("malformed header: {line}")));
        };
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    Ok(RequestHead {
        method: method.to_string(),
        target: target.to_string(),
        version: version.to_string(),
        headers,
    })
}

/// Split an `http://host[:port]/path` absolute URI.
pub fn split_absolute_uri(target: &str) -> SeamResult<(String, u16, String)> {
    let rest = target.strip_prefix("http://").ok_or_else(|| {
        SeamError::Invalid(format!(
            "proxy only accepts absolute http:// URIs for plaintext requests: {target}"
        ))
    })?;
    let (authority, path) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((host, port)) => (
            host,
            port.parse::<u16>()
                .map_err(|_| SeamError::Invalid(format!("bad port in {target}")))?,
        ),
        None => (authority, 80),
    };
    if host.is_empty() {
        return Err(SeamError::Invalid(format!("missing host in {target}")));
    }
    Ok((host.to_string(), port, path.to_string()))
}

fn forbidden(reason: &str) -> String {
    format!(
        "HTTP/1.1 403 Forbidden\r\nX-Aria-Egress: denied\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reason.len(),
        reason
    )
}

async fn tunnel(
    mut client: TcpStream,
    head: &RequestHead,
    _leftover: Vec<u8>,
    gateway: Arc<EgressGateway>,
) -> SeamResult<()> {
    let (host, port) = head.target.rsplit_once(':').ok_or_else(|| {
        SeamError::Invalid(format!("CONNECT needs host:port, got {}", head.target))
    })?;
    let port: u16 = port
        .parse()
        .map_err(|_| SeamError::Invalid(format!("bad CONNECT port: {port}")))?;

    if let EgressDecision::Deny { reason } = gateway.authorize(host).await? {
        send_bytes(&mut client, forbidden(&reason).as_bytes(), "write 403").await?;
        return Ok(());
    }

    let mut upstream = connect(host, port).await?;
    send_bytes(
        &mut client,
        b"HTTP/1.1 200 Connection Established\r\n\r\n",
        "write 200",
    )
    .await?;
    relay(&mut client, &mut upstream, &format!("tunnel {host}:{port}")).await
}

async fn forward_http(
    mut client: TcpStream,
    head: &RequestHead,
    leftover: Vec<u8>,
    gateway: Arc<EgressGateway>,
) -> SeamResult<()> {
    let (host, port, path) = split_absolute_uri(&head.target)?;
    if let EgressDecision::Deny { reason } = gateway.authorize(&host).await? {
        send_bytes(&mut client, forbidden(&reason).as_bytes(), "write 403").await?;
        return Ok(());
    }

    let mut headers = head.headers.clone();
    gateway.substitute(&host, &path, &mut headers).await?;

    let mut upstream = connect(&host, port).await?;
    let mut request = format!("{} {} {}\r\n", head.method, path, head.version);
    for (name, value) in &headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    send_bytes(&mut upstream, request.as_bytes(), "write upstream request").await?;
    if !leftover.is_empty() {
        send_bytes(&mut upstream, &leftover, "write upstream body").await?;
    }
    relay(
        &mut client,
        &mut upstream,
        &format!("forward {host}:{port}"),
    )
    .await
}

async fn connect(host: &str, port: u16) -> SeamResult<TcpStream> {
    connect_with_timeout(host, port, CONNECT_TIMEOUT).await
}

/// The real dial, kept as a function so tests can inject a future that never
/// resolves instead of relying on kernel backlog timing.
async fn tcp_connect(host: String, port: u16) -> std::io::Result<TcpStream> {
    TcpStream::connect((host.as_str(), port)).await
}

/// One dial with an explicit deadline. Keeping the timeout at a single call
/// site, with the dial itself injectable, makes the timeout branch testable
/// without a saturated loopback backlog (whose accept queue is kernel-dependent).
async fn connect_with<F, Fut>(
    host: &str,
    port: u16,
    timeout: Duration,
    dial: F,
) -> SeamResult<TcpStream>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = std::io::Result<TcpStream>>,
{
    match tokio::time::timeout(timeout, dial(host.to_string(), port)).await {
        Ok(Ok(stream)) => Ok(stream),
        Ok(Err(e)) => Err(SeamError::Io(format!("connect {host}:{port}: {e}"))),
        Err(_) => Err(SeamError::Io(timeout_message(host, port))),
    }
}

async fn connect_with_timeout(host: &str, port: u16, timeout: Duration) -> SeamResult<TcpStream> {
    connect_with(host, port, timeout, tcp_connect).await
}

/// Message for a dial that ran out of time: the address, never the payload.
fn timeout_message(host: &str, port: u16) -> String {
    format!("connect {host}:{port} timed out after {CONNECT_TIMEOUT:?}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PolicyStore;
    use agent_core::seams::{EgressMode, EgressPolicy, HostPattern, NoAudit, NoCredentials};

    fn gateway(policy: EgressPolicy) -> Arc<EgressGateway> {
        Arc::new(EgressGateway::new(
            Arc::new(PolicyStore::new(policy)),
            Arc::new(NoCredentials),
            Arc::new(NoAudit::new()),
            "tenant-a",
        ))
    }

    fn allow_localhost() -> EgressPolicy {
        EgressPolicy::new(
            5,
            EgressMode::Allowlist,
            vec![HostPattern::parse("127.0.0.1").unwrap()],
            Vec::new(),
        )
    }

    async fn echo_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    let mut buffer = [0u8; 2048];
                    let _ = stream.read(&mut buffer).await;
                    let _ = stream
                        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                        .await;
                });
            }
        });
        (addr, handle)
    }

    async fn send(proxy: SocketAddr, request: &str) -> String {
        let mut client = TcpStream::connect(proxy).await.unwrap();
        client.write_all(request.as_bytes()).await.unwrap();
        let mut out = Vec::new();
        client.read_to_end(&mut out).await.unwrap();
        String::from_utf8_lossy(&out).to_string()
    }

    #[test]
    fn request_heads_parse_strictly() {
        let head = parse_head(
            "GET http://a.com/x HTTP/1.1\r\nHost: a.com\r\nAuthorization: Bearer x\r\n\r\n",
        )
        .unwrap();
        assert_eq!(head.method, "GET");
        assert_eq!(head.target, "http://a.com/x");
        assert_eq!(head.header("authorization"), Some("Bearer x"));
        assert_eq!(head.header("missing"), None);
        assert!(matches!(parse_head(""), Err(SeamError::Invalid(_))));
        // Whitespace only: no method token at all.
        assert!(matches!(parse_head(" "), Err(SeamError::Invalid(_))));
        assert!(matches!(parse_head("GET"), Err(SeamError::Invalid(_))));
        assert!(matches!(
            parse_head("GET / HTTP/1.1\r\nbadheader"),
            Err(SeamError::Invalid(_))
        ));
    }

    #[test]
    fn absolute_uris_split_host_port_and_path() {
        assert_eq!(
            split_absolute_uri("http://a.com/x").unwrap(),
            ("a.com".to_string(), 80, "/x".to_string())
        );
        assert_eq!(
            split_absolute_uri("http://a.com:8080").unwrap(),
            ("a.com".to_string(), 8080, "/".to_string())
        );
        assert!(split_absolute_uri("https://a.com/x").is_err());
        assert!(split_absolute_uri("http://a.com:notaport/x").is_err());
        assert!(split_absolute_uri("http:///x").is_err());
    }

    struct RefusingFactory;

    #[async_trait::async_trait]
    impl GatewayFactory for RefusingFactory {
        async fn for_peer(&self, _peer: SocketAddr) -> SeamResult<Arc<EgressGateway>> {
            Err(SeamError::Unavailable("unknown peer".into()))
        }
    }

    struct AllowingFactory {
        gateway: Arc<EgressGateway>,
        seen: Arc<std::sync::Mutex<Vec<SocketAddr>>>,
    }

    #[async_trait::async_trait]
    impl GatewayFactory for AllowingFactory {
        async fn for_peer(&self, peer: SocketAddr) -> SeamResult<Arc<EgressGateway>> {
            self.seen.lock().unwrap().push(peer);
            Ok(self.gateway.clone())
        }
    }

    #[tokio::test]
    async fn an_unidentified_peer_is_refused_before_any_decision() {
        let proxy = LocalForwardProxy::bind_with("127.0.0.1:0", Arc::new(RefusingFactory))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let response = send(
            addr,
            "GET http://127.0.0.1:1/x HTTP/1.1\r\nHost: 127.0.0.1:1\r\n\r\n",
        )
        .await;
        assert!(response.starts_with("HTTP/1.1 403"), "{response}");
        assert!(response.contains("could not identify peer"), "{response}");
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn bind_with_uses_the_factory_gateway_per_connection() {
        let (upstream, _echo) = echo_server().await;
        let gateway = gateway(allow_localhost());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = LocalForwardProxy::bind_with(
            "127.0.0.1:0",
            Arc::new(AllowingFactory {
                gateway,
                seen: seen.clone(),
            }),
        )
        .await
        .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let response = send(
            addr,
            &format!(
                "GET http://127.0.0.1:{}/x HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n",
                upstream.port()
            ),
        )
        .await;
        assert!(response.contains("200 OK"), "{response}");
        assert_eq!(seen.lock().unwrap().len(), 1);
        server.await.unwrap().unwrap();
    }

    #[test]
    fn head_terminator_is_found() {
        assert_eq!(find_head_end(b"GET / HTTP/1.1\r\n\r\nBODY"), Some(18));
        assert_eq!(find_head_end(b"GET /"), None);
    }

    #[tokio::test]
    async fn allowed_plaintext_requests_are_proxied() {
        let (upstream, _handle) = echo_server().await;
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        assert!(proxy.proxy_url().starts_with("http://127.0.0.1:"));
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let response = send(
            addr,
            &format!("GET http://{upstream}/x HTTP/1.1\r\nHost: {upstream}\r\n\r\n"),
        )
        .await;
        assert!(response.contains("200 OK"), "{response}");
        assert!(response.contains("hello"));
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn denied_plaintext_requests_never_reach_upstream() {
        let (upstream, _handle) = echo_server().await;
        let proxy = LocalForwardProxy::bind(
            "127.0.0.1:0",
            gateway(EgressPolicy::new(
                1,
                EgressMode::Allowlist,
                vec![HostPattern::parse("example.com").unwrap()],
                Vec::new(),
            )),
        )
        .await
        .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let response = send(
            addr,
            &format!("GET http://{upstream}/x HTTP/1.1\r\nHost: {upstream}\r\n\r\n"),
        )
        .await;
        assert!(response.contains("403 Forbidden"), "{response}");
        assert!(response.contains("X-Aria-Egress: denied"));
        assert!(
            response.contains("not on the egress allowlist"),
            "{response}"
        );
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn connect_tunnels_when_allowed_and_is_refused_when_not() {
        let (upstream, _handle) = echo_server().await;

        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(format!("CONNECT {upstream} HTTP/1.1\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut handshake = [0u8; 64];
        let read = client.read(&mut handshake).await.unwrap();
        assert!(String::from_utf8_lossy(&handshake[..read]).contains("200 Connection Established"));
        // Close the tunnel before waiting for the handler: `copy_bidirectional`
        // only finishes when one side hangs up.
        drop(client);
        server.await.unwrap().unwrap();

        let proxy = LocalForwardProxy::bind(
            "127.0.0.1:0",
            gateway(EgressPolicy::new(
                1,
                EgressMode::Allowlist,
                vec![HostPattern::parse("example.com").unwrap()],
                Vec::new(),
            )),
        )
        .await
        .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let response = send(addr, &format!("CONNECT {upstream} HTTP/1.1\r\n\r\n")).await;
        assert!(response.contains("403 Forbidden"), "{response}");
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn a_malformed_connect_target_is_rejected() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT noport HTTP/1.1\r\n\r\n")
            .await
            .unwrap();
        let mut out = Vec::new();
        let _ = client.read_to_end(&mut out).await;
        assert!(server.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn authorize_audits_both_outcomes() {
        let audit = Arc::new(NoAudit::new());
        let gateway = EgressGateway::new(
            Arc::new(PolicyStore::new(allow_localhost())),
            Arc::new(NoCredentials),
            audit.clone(),
            "tenant-a",
        )
        .with_session("s1");
        assert!(gateway.authorize("127.0.0.1").await.unwrap().is_allowed());
        assert!(!gateway.authorize("evil.com").await.unwrap().is_allowed());
        assert_eq!(audit.dropped(), 2);
        let mut headers = Vec::new();
        let report = gateway
            .substitute("127.0.0.1", "/x", &mut headers)
            .await
            .unwrap();
        assert_eq!(report.applied, 0);
    }

    #[tokio::test]
    async fn bind_reports_an_unusable_address() {
        // A space makes the address invalid on every platform; `256.0.0.1`
        // depends on the resolver and can bind on Windows.
        assert!(
            LocalForwardProxy::bind("not a host:0", gateway(allow_localhost()))
                .await
                .is_err()
        );
    }

    /// A real JSONL sink whose directory has been removed: every append fails,
    /// which is exactly what a full disk looks like.
    fn broken_sink() -> Arc<crate::audit::JsonlAuditSink> {
        let dir = tempfile::tempdir().unwrap();
        let sink = crate::audit::JsonlAuditSink::new(dir.path(), 0).unwrap();
        std::fs::remove_dir_all(dir.path()).unwrap();
        Arc::new(sink)
    }

    #[tokio::test]
    async fn audit_failures_never_change_a_decision() {
        let gateway = EgressGateway::new(
            Arc::new(PolicyStore::new(allow_localhost())),
            Arc::new(NoCredentials),
            broken_sink(),
            "tenant-a",
        );
        // The sink is broken, so the warn branch fires; the decision stands.
        let decision = gateway.authorize("127.0.0.1").await.unwrap();
        assert!(decision.is_allowed());
    }

    #[tokio::test]
    async fn a_substitution_is_audited_and_survives_a_broken_sink() {
        std::env::set_var("PROXY_TEST_CRED_KEY", "proxy-passphrase");
        let dir = tempfile::tempdir().unwrap();
        let store = crate::credentials::FileCredentialStore::open(
            dir.path().join("creds"),
            "PROXY_TEST_CRED_KEY",
            0,
        )
        .unwrap();
        store
            .rotate("openai", agent_core::seams::SecretValue::new("sk-live"))
            .await
            .unwrap();
        store
            .put_stub(agent_core::seams::SecretStub {
                id: "s1".into(),
                host: HostPattern::parse("127.0.0.1").unwrap(),
                path_prefix: Some("/v1/".into()),
                header: "authorization".into(),
                value_template: "Bearer ${credential}".into(),
                credential: "openai".into(),
            })
            .await
            .unwrap();
        let audit_dir = tempfile::tempdir().unwrap();
        let sink = Arc::new(crate::audit::JsonlAuditSink::new(audit_dir.path(), 0).unwrap());
        let gateway = EgressGateway::new(
            Arc::new(PolicyStore::new(allow_localhost())),
            Arc::new(store),
            sink.clone(),
            "tenant-a",
        );
        let mut headers = Vec::new();
        let report = gateway
            .substitute("127.0.0.1", "/v1/x?key=1", &mut headers)
            .await
            .unwrap();
        assert_eq!(report.applied, 1);
        // The audit line records the path, never the query or the secret value.
        let recent = sink.recent(10).await.unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].target, "127.0.0.1");
        let metadata = recent[0].metadata.clone();
        assert_eq!(metadata["path"], "/v1/x");
        assert!(!metadata.to_string().contains("sk-live"));

        // The same store, but the sink cannot be written: the substitution still
        // reaches the caller (a broken audit trail must not break the request).
        let store = crate::credentials::FileCredentialStore::open(
            dir.path().join("creds"),
            "PROXY_TEST_CRED_KEY",
            0,
        )
        .unwrap();
        let gateway = EgressGateway::new(
            Arc::new(PolicyStore::new(allow_localhost())),
            Arc::new(store),
            broken_sink(),
            "tenant-a",
        );
        let mut headers = Vec::new();
        assert_eq!(
            gateway
                .substitute("127.0.0.1", "/v1/x", &mut headers)
                .await
                .unwrap()
                .applied,
            1
        );
    }

    #[tokio::test]
    async fn serve_accepts_several_connections_and_survives_a_bad_one() {
        let (upstream, _echo) = echo_server().await;
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(proxy.serve());
        for _ in 0..2 {
            let response = send(
                addr,
                &format!("GET http://{upstream}/a HTTP/1.1\r\nHost: {upstream}\r\n\r\n"),
            )
            .await;
            assert!(response.contains("hello"), "{response}");
        }
        // A broken connection ends with a logged error; the listener keeps going.
        let _ = send(addr, "GET\r\n\r\n").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        let response = send(
            addr,
            &format!("GET http://{upstream}/b HTTP/1.1\r\nHost: {upstream}\r\n\r\n"),
        )
        .await;
        assert!(response.contains("hello"), "{response}");
        server.abort();
    }

    #[tokio::test]
    async fn a_request_body_that_arrived_with_the_head_is_forwarded() {
        let (upstream, _echo) = echo_server().await;
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        // The body is already in the socket when the head ends: it must not be
        // dropped on the floor.
        let response = send(
            addr,
            &format!(
                "POST http://{upstream}/x HTTP/1.1\r\nHost: {upstream}\r\nContent-Length: 5\r\n\r\nhello"
            ),
        )
        .await;
        assert!(response.contains("hello"), "{response}");
        assert!(server.await.unwrap().is_ok());
    }

    #[test]
    fn a_timeout_message_names_the_address_and_never_a_payload() {
        let message = timeout_message("example.com", 443);
        assert!(message.contains("example.com:443"), "{message}");
        assert!(message.contains("timed out"), "{message}");
    }

    #[tokio::test]
    async fn an_endless_head_is_cut_off_instead_of_buffered() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        // No header terminator at all: the proxy must stop at the limit. The
        // client write may be cut short by the reset; only the proxy's verdict
        // matters here, and Windows closes the socket earlier than Linux.
        let _ = client.write_all(&vec![b'x'; MAX_HEADER_BYTES + 16]).await;
        let result = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_err(), "expected a refused head, got {result:?}");
    }

    #[tokio::test]
    async fn a_client_that_hangs_up_mid_head_is_an_error() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(b"GET http://127.0.0.1").await.unwrap();
        client.shutdown().await.unwrap();
        let result = server.await.unwrap();
        assert!(result.is_err(), "half a head is not a request");
    }

    #[tokio::test]
    async fn a_request_line_without_a_target_is_refused() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        // "GET" alone is not a request line.
        let _ = send(addr, "GET\r\n\r\n").await;
        assert!(server.await.unwrap().is_err());
    }

    #[tokio::test]
    async fn a_connect_port_that_is_not_a_number_is_refused() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        let _ = send(addr, "CONNECT 127.0.0.1:notaport HTTP/1.1\r\n\r\n").await;
        let err = server.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("bad CONNECT port"), "{err}");
    }

    #[tokio::test]
    async fn a_connect_to_a_closed_port_is_an_io_error() {
        let proxy = LocalForwardProxy::bind("127.0.0.1:0", gateway(allow_localhost()))
            .await
            .unwrap();
        let addr = proxy.addr();
        let server = tokio::spawn(async move { proxy.serve_one().await });
        // Port 1 is allowed by the policy but nothing listens there: the upstream
        // is dialled before the 200 is written, so the client sees a closed
        // connection and the proxy reports the failure.
        let response = send(addr, "CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n").await;
        assert!(
            response.is_empty(),
            "no header for a failed dial: {response}"
        );
        assert!(server.await.unwrap().is_err());
    }

    /// A peer that closes with RST instead of FIN: `SO_LINGER = 0` makes the
    /// kernel reject the connection, so the other side sees a real `io::Error`
    /// rather than a clean EOF. Loopback only, no external dependency. The
    /// coverage gate runs on Linux, where the RST timing is deterministic.
    #[cfg(unix)]
    fn rst_peer(addr: SocketAddr) -> std::net::TcpStream {
        let peer = std::net::TcpStream::connect(addr).unwrap();
        socket2::SockRef::from(&peer)
            .set_linger(Some(Duration::ZERO))
            .unwrap();
        peer
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_reset_peer_is_reported_while_reading_the_request_head() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            tokio::time::sleep(Duration::from_millis(100)).await;
            read_head(&mut stream).await
        });
        let peer = rst_peer(addr);
        accepted_rx.await.unwrap();
        drop(peer);
        let err = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("read request head"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_reset_peer_is_reported_while_writing() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            tokio::time::sleep(Duration::from_millis(100)).await;
            send_bytes(&mut stream, b"HTTP/1.1 403 Forbidden\r\n\r\n", "write 403").await
        });
        let peer = rst_peer(addr);
        accepted_rx.await.unwrap();
        drop(peer);
        let err = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("write 403"), "{err}");
    }

    /// A listener whose fd has been torn down: both address lookup and accept
    /// fail, which is what a proxy sees when its socket is closed underneath it.
    struct FailingListener;

    #[async_trait::async_trait]
    impl ProxyListener for FailingListener {
        async fn accept(&self) -> std::io::Result<(TcpStream, SocketAddr)> {
            Err(std::io::Error::other("listener torn down"))
        }

        fn local_addr(&self) -> std::io::Result<SocketAddr> {
            Err(std::io::Error::other("no local address"))
        }
    }

    #[cfg(test)]
    fn proxy_with_listener(listener: Box<dyn ProxyListener>) -> LocalForwardProxy {
        LocalForwardProxy {
            listener,
            addr: "127.0.0.1:0".parse().unwrap(),
            factory: Arc::new(SingleGateway(gateway(allow_localhost()))),
        }
    }

    #[tokio::test]
    async fn a_listener_without_an_address_is_refused() {
        let err = LocalForwardProxy::from_listener(
            Box::new(FailingListener),
            Arc::new(SingleGateway(gateway(allow_localhost()))),
        )
        .err()
        .expect("a listener without an address must be refused");
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("proxy local_addr"), "{err}");
    }

    #[tokio::test]
    async fn a_failed_accept_is_reported_by_serve_one() {
        let proxy = proxy_with_listener(Box::new(FailingListener));
        let err = proxy.serve_one().await.unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("proxy accept"), "{err}");
    }

    #[tokio::test]
    async fn serve_survives_repeated_accept_failures() {
        let proxy = proxy_with_listener(Box::new(FailingListener));
        let server = tokio::spawn(proxy.serve());
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert!(
            !server.is_finished(),
            "accept errors must not stop the listener"
        );
        server.abort();
        let _ = server.await;
    }

    /// A dial that never resolves: the deadline is the only way out, so the
    /// timeout branch is deterministic on every kernel (a saturated accept
    /// queue may still accept another SYN).
    async fn pending_dial(_host: String, _port: u16) -> std::io::Result<TcpStream> {
        std::future::pending().await
    }

    #[tokio::test]
    async fn a_dial_that_outlives_the_deadline_is_reported() {
        let err = connect_with("127.0.0.1", 1, Duration::from_millis(20), pending_dial)
            .await
            .unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("timed out"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_reset_peer_is_reported_during_relay() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let upstream_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream_listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut client, _) = listener.accept().await.unwrap();
            let _upstream_peer = TcpStream::connect(upstream_addr).await.unwrap();
            let (mut upstream, _) = upstream_listener.accept().await.unwrap();
            let _ = accepted_tx.send(());
            tokio::time::sleep(Duration::from_millis(100)).await;
            relay(&mut client, &mut upstream, "tunnel 127.0.0.1:1").await
        });
        let peer = rst_peer(addr);
        accepted_rx.await.unwrap();
        drop(peer);
        let err = tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        assert!(matches!(err, SeamError::Io(_)), "{err:?}");
        assert!(err.to_string().contains("tunnel 127.0.0.1:1"), "{err}");
    }
}
