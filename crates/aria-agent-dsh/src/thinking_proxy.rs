//! Local reverse proxy that strips Anthropic `redacted_thinking` content blocks.
//!
//! `deepseek-official` (the dsh adapter) cannot translate a signed
//! `redacted_thinking` block: it accepts one in the first turn, persists it in
//! session history, then throws `UNSUPPORTED_CONTENT` while replaying that
//! history on the second turn. Filtering at this boundary means dsh never sees
//! the block in the first place, so multi-turn tool loops keep running.
//!
//! Only `redacted_thinking` is removed. `thinking` blocks are kept on purpose:
//! DeepSeek's Anthropic-compatible layer requires `content[].thinking` to be
//! passed back verbatim in thinking mode, and stripping it would trade one 400
//! for another. Thinking therefore stays display-only **end to end** — the same
//! trade-off OpenCode makes (`ReasoningContent` is never replayed).
//!
//! This is a compatibility downgrade for Anthropic-compatible gateways, not a
//! general fix for Anthropic's extended-thinking contract; the real fix belongs
//! in dsh's adapter.
//!
//! Wiring: dsh's `DEEPSEEK_BASE_URL` is pointed at [`ThinkingFilterProxy::local_base_url`]
//! (the root, without `/anthropic/v1`) and the proxy forwards to the real
//! gateway. See `docs/adr/0019-dsh-runtime.md`.

use std::collections::BTreeSet;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use agent_core::CoreError;
use futures::StreamExt;
use reqwest::header::{
    HeaderMap, HeaderName, HeaderValue, ACCEPT_ENCODING, CONTENT_ENCODING, CONTENT_LENGTH,
    CONTENT_TYPE,
};
use reqwest::{Client, Method, StatusCode};
use serde_json::{json, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::runtime::dsh_base_url;

/// The one content-block type the adapter cannot represent.
const REDACTED_THINKING: &str = "redacted_thinking";

/// Value that turns the filter on (`DSH_THINKING_FILTER`).
pub const FILTER_ENABLE: &str = "enable";
/// Value that turns the filter off (dsh talks to the gateway directly).
pub const FILTER_DISABLE: &str = "disable";

/// Parse `DSH_THINKING_FILTER`: `enable` (default when unset/empty) or
/// `disable`. Anything else is a configuration error — never a silent default.
pub fn parse_filter_mode(raw: Option<&str>) -> Result<bool, CoreError> {
    let Some(raw) = raw else {
        return Ok(true);
    };
    let value = raw.trim().to_ascii_lowercase();
    match value.as_str() {
        "" | FILTER_ENABLE => Ok(true),
        FILTER_DISABLE => Ok(false),
        other => Err(CoreError::Config(format!(
            "unknown DSH_THINKING_FILTER value `{other}` (expected enable|disable)"
        ))),
    }
}

fn is_redacted(block: &Value) -> bool {
    block
        .get("type")
        .and_then(Value::as_str)
        .is_some_and(|kind| kind == REDACTED_THINKING)
}

fn strip_from_content(content: Option<&mut Value>) -> usize {
    let Some(Value::Array(items)) = content else {
        return 0;
    };
    let before = items.len();
    items.retain(|block| !is_redacted(block));
    before - items.len()
}

/// Request direction: remove `redacted_thinking` blocks from every
/// `messages[].content[]` array and from a block-array `system` field, in
/// place. Returns the number of blocks removed.
///
/// Plain-string content is left alone; malformed shapes are skipped rather than
/// rejected so unrelated gateway quirks do not break a turn.
pub fn strip_redacted_from_messages_json(body: &mut Value) -> usize {
    let mut removed = 0;
    if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
        for message in messages {
            removed += strip_from_content(message.get_mut("content"));
        }
    }
    removed + strip_from_content(body.get_mut("system"))
}

/// Response direction for non-streaming JSON: remove `redacted_thinking` from
/// both a top-level `content[]` and a nested `message.content[]`.
pub fn strip_redacted_from_response_json(body: &mut Value) -> usize {
    let mut removed = strip_from_content(body.get_mut("content"));
    if let Some(message) = body.get_mut("message") {
        removed += strip_from_content(message.get_mut("content"));
    }
    removed
}

/// Map an original content-block index to the index it has after the redacted
/// blocks before it were removed, keeping the remaining sequence contiguous.
fn remap_index(index: u64, suppressed: &BTreeSet<u64>) -> u64 {
    index - suppressed.iter().take_while(|&&s| s < index).count() as u64
}

/// Split one SSE frame into its non-`data:` lines and its parsed JSON payload.
/// Returns `None` when there is no `data:` line or it is not JSON — such frames
/// are passed through untouched.
fn parse_sse_frame(frame: &str) -> Option<(Vec<String>, Value)> {
    let mut prefix = Vec::new();
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
        } else if !line.is_empty() {
            prefix.push(line.to_string());
        }
    }
    if data_lines.is_empty() {
        return None;
    }
    serde_json::from_str(&data_lines.join("\n"))
        .ok()
        .map(|v| (prefix, v))
}

fn rebuild_frame(prefix: &[String], data: &Value) -> String {
    let mut out = String::new();
    for line in prefix {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("data: ");
    out.push_str(&data.to_string());
    out
}

/// Stateful filter for Anthropic Messages SSE frames.
///
/// Feed complete frames (separated by a blank line) in wire order. A redacted
/// block's `content_block_start`, its deltas and its `content_block_stop` are
/// dropped; later block indices are decremented so dsh sees a contiguous
/// sequence, semantically identical to "that block never existed".
#[derive(Debug, Default)]
pub struct SseRedactedFilter {
    suppressed: BTreeSet<u64>,
}

impl SseRedactedFilter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of redacted blocks suppressed so far (for debug logging only).
    pub fn removed_count(&self) -> usize {
        self.suppressed.len()
    }

    /// Filter one complete SSE frame. `None` means the whole frame is dropped.
    pub fn filter_frame(&mut self, frame: &str) -> Option<String> {
        let Some((prefix, mut data)) = parse_sse_frame(frame) else {
            return Some(frame.to_string());
        };
        let kind = data.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "content_block_start" => {
                let Some(index) = data.get("index").and_then(Value::as_u64) else {
                    return Some(frame.to_string());
                };
                if data.get("content_block").is_some_and(is_redacted) {
                    self.suppressed.insert(index);
                    return None;
                }
                let new_index = remap_index(index, &self.suppressed);
                if new_index == index {
                    return Some(frame.to_string());
                }
                data["index"] = json!(new_index);
            }
            "content_block_delta" | "content_block_stop" => {
                let Some(index) = data.get("index").and_then(Value::as_u64) else {
                    return Some(frame.to_string());
                };
                if self.suppressed.contains(&index) {
                    return None;
                }
                let new_index = remap_index(index, &self.suppressed);
                if new_index == index {
                    return Some(frame.to_string());
                }
                data["index"] = json!(new_index);
            }
            "message_start" => {
                let removed = data
                    .get_mut("message")
                    .and_then(|m| m.get_mut("content"))
                    .map(|content| strip_from_content(Some(content)))
                    .unwrap_or(0);
                if removed == 0 {
                    return Some(frame.to_string());
                }
            }
            _ => return Some(frame.to_string()),
        }
        Some(rebuild_frame(&prefix, &data))
    }
}

/// Bounds for the proxy. Request heads/bodies and SSE frames are bounded so a
/// misbehaving child or gateway cannot exhaust the process (fail closed).
const MAX_REQUEST_HEAD_BYTES: usize = 64 * 1024;
const MAX_REQUEST_BODY_BYTES: usize = 32 * 1024 * 1024;
const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
/// A stalled upstream may not hold a turn forever; streaming reads get the same
/// bound, so a live (but slow) stream keeps flowing while a dead one is cut.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const READ_TIMEOUT: Duration = Duration::from_secs(120);

/// Format a `CoreError::Config` from a fallible step. The mapping lives in one
/// helper on purpose: a per-call-site `map_err(|e| ...)` closure is a region
/// that stays unexecuted on the happy path (see `docs/coverage.md`).
fn config_err<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> CoreError {
    move |error| CoreError::Config(format!("{context}: {error}"))
}

/// The same for `CoreError::Model`.
fn model_err<E: std::fmt::Display>(context: &'static str) -> impl FnOnce(E) -> CoreError {
    move |error| CoreError::Model(format!("{context}: {error}"))
}

/// Build the client used for upstream calls. `rustls-tls` uses the compiled-in
/// webpki-roots 1.0.10+ store, never the system CA bundle (eMudhra/emSign CA
/// compromise, 2026-10).
fn build_client() -> Result<Client, CoreError> {
    build_client_with(|| {
        Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
    })
}

/// Seam for [`build_client`]: the TLS backend cannot be made to fail on demand.
fn build_client_with<B, E>(build: B) -> Result<Client, CoreError>
where
    B: FnOnce() -> Result<Client, E>,
    E: std::fmt::Display,
{
    build().map_err(config_err("cannot build proxy TLS client"))
}

/// Seam for the loopback listener: `bind`, `local_addr` and `accept` failures
/// cannot be produced against a real ephemeral socket, so tests inject one.
#[async_trait::async_trait]
trait ProxyListener: Send + 'static {
    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)>;
    fn local_addr(&self) -> io::Result<SocketAddr>;
}

#[async_trait::async_trait]
impl ProxyListener for TcpListener {
    async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
        TcpListener::accept(self).await
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        TcpListener::local_addr(self)
    }
}

/// A loopback reverse proxy that sits between dsh and the real gateway.
///
/// dsh's `DEEPSEEK_BASE_URL` is pointed at [`ThinkingFilterProxy::local_base_url`]
/// (the root, without `/anthropic/v1`); [`crate::DshConfig::with_gateway_env`]
/// appends the API prefix. The proxy strips redacted blocks in both directions
/// before dsh can persist one into session history.
#[derive(Debug, Clone)]
pub struct ThinkingFilterProxy {
    addr: SocketAddr,
}

impl ThinkingFilterProxy {
    /// Bind an ephemeral loopback port and start the accept loop. A failure to
    /// bind or build the TLS client is a hard error (fail closed).
    pub async fn start(upstream_root: &str) -> Result<Self, CoreError> {
        Self::start_with(upstream_root, || TcpListener::bind("127.0.0.1:0")).await
    }

    /// Bind at an explicit address.
    ///
    /// A sandboxed harness cannot reach the control plane's `127.0.0.1`, so a
    /// containerised deployment binds somewhere reachable (`0.0.0.0:<port>` on a
    /// governed network) and tells dsh that address through
    /// `DSH_THINKING_FILTER_URL` — the bound address and the advertised one are
    /// deliberately separate facts.
    pub async fn start_on(upstream_root: &str, addr: SocketAddr) -> Result<Self, CoreError> {
        Self::start_with(upstream_root, || TcpListener::bind(addr)).await
    }

    /// `bind` is a seam so tests can exercise the binding and address failures.
    async fn start_with<L, B, F>(upstream_root: &str, bind: B) -> Result<Self, CoreError>
    where
        L: ProxyListener,
        B: FnOnce() -> F,
        F: std::future::Future<Output = io::Result<L>>,
    {
        let upstream_base = dsh_base_url(upstream_root);
        if upstream_base.is_empty() {
            return Err(CoreError::Config(
                "thinking filter proxy: empty upstream root".into(),
            ));
        }
        let client = build_client()?;
        let listener = bind()
            .await
            .map_err(config_err("cannot bind thinking filter proxy"))?;
        let addr = listener
            .local_addr()
            .map_err(config_err("cannot read proxy address"))?;
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(serve_connection(
                            stream,
                            client.clone(),
                            upstream_base.clone(),
                        ));
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "thinking filter proxy accept failed");
                        break;
                    }
                }
            }
        });
        Ok(Self { addr })
    }

    /// Root URL for `DshConfig::with_gateway_env` (no `/anthropic/v1` suffix).
    pub fn local_base_url(&self) -> String {
        format!("http://{}", self.addr)
    }
}

fn is_hop_by_hop(name: &str) -> bool {
    [
        "connection",
        "proxy-connection",
        "keep-alive",
        "transfer-encoding",
        "upgrade",
        "te",
        "trailer",
        "host",
    ]
    .iter()
    .any(|hop| name.eq_ignore_ascii_case(hop))
}

/// Map an inbound path to the upstream URL without duplicating `/v1`.
fn upstream_url(base: &str, target: &str) -> Result<String, CoreError> {
    let suffix = target
        .strip_prefix("/anthropic/v1")
        .or_else(|| target.strip_prefix("/v1"))
        .unwrap_or(target);
    if suffix.is_empty() {
        return Ok(base.trim_end_matches('/').to_string());
    }
    if !suffix.starts_with('/') {
        return Err(CoreError::Model(format!("invalid proxy target: {target}")));
    }
    Ok(format!("{}{}", base.trim_end_matches('/'), suffix))
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

async fn read_request_head<S>(stream: &mut S) -> Result<(String, Vec<u8>), CoreError>
where
    S: AsyncRead + Unpin,
{
    let mut buf = Vec::new();
    loop {
        if buf.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(CoreError::Config("proxy request head too large".into()));
        }
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(model_err("proxy read failed"))?;
        if n == 0 {
            return Err(CoreError::Model(
                "connection closed before request head".into(),
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let leftover = buf[pos + 4..].to_vec();
            return Ok((head, leftover));
        }
    }
}

type ParsedRequestHead = (String, String, Vec<(String, String)>);

fn parse_request_head(head: &str) -> Result<ParsedRequestHead, CoreError> {
    let mut lines = head.split("\r\n");
    // `split` always yields at least one item (possibly empty), so an empty head
    // falls through to the "missing request method" error below.
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| CoreError::Model("missing request method".into()))?
        .to_string();
    let target = parts
        .next()
        .ok_or_else(|| CoreError::Model("missing request target".into()))?
        .to_string();
    let headers = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            Some((name.trim().to_string(), value.trim().to_string()))
        })
        .collect();
    Ok((method, target, headers))
}

/// Render the downstream response head. `content-length` is recomputed and
/// `content-encoding` is never forwarded (the proxy forces identity encoding so
/// it can always inspect bodies).
fn build_head(status: StatusCode, headers: &HeaderMap, content_length: Option<usize>) -> String {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (name, value) in headers {
        if is_hop_by_hop(name.as_str()) || name == CONTENT_LENGTH || name == CONTENT_ENCODING {
            continue;
        }
        if let Ok(value) = value.to_str() {
            head.push_str(name.as_str());
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
    }
    head.push_str("Connection: close\r\n");
    if let Some(len) = content_length {
        head.push_str(&format!("Content-Length: {len}\r\n"));
    }
    head.push_str("\r\n");
    head
}

async fn write_head<S>(
    stream: &mut S,
    status: StatusCode,
    headers: &HeaderMap,
    content_length: Option<usize>,
) -> Result<(), CoreError>
where
    S: AsyncWrite + Unpin,
{
    let head = build_head(status, headers, content_length);
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(model_err("proxy write failed"))
}

/// Head and body go out in one write so a slow client cannot split the response.
/// Read the request body up to `content_length`, appending to whatever arrived
/// alongside the head. EOF before the declared length means the client vanished
/// mid-body, which is a hard error (never a silently truncated prompt).
async fn read_body<S>(
    stream: &mut S,
    mut body: Vec<u8>,
    content_length: usize,
) -> Result<Vec<u8>, CoreError>
where
    S: AsyncRead + Unpin,
{
    while body.len() < content_length {
        let mut chunk = [0u8; 8192];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(model_err("proxy body read failed"))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() != content_length {
        return Err(CoreError::Model("incomplete proxy request body".into()));
    }
    Ok(body)
}

async fn write_response<S>(
    stream: &mut S,
    status: StatusCode,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), CoreError>
where
    S: AsyncWrite + Unpin,
{
    let mut out = build_head(status, headers, Some(body.len())).into_bytes();
    out.extend_from_slice(body);
    stream
        .write_all(&out)
        .await
        .map_err(model_err("proxy response write failed"))
}

fn split_sse_frame(buffer: &[u8]) -> Option<(Vec<u8>, usize)> {
    let crlf = buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|pos| (pos, 4));
    let lf = buffer
        .windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| (pos, 2));
    let (end, sep) = match (crlf, lf) {
        (Some(a), Some(b)) => {
            if a.0 <= b.0 {
                a
            } else {
                b
            }
        }
        (Some(a), None) => a,
        (None, Some(b)) => b,
        (None, None) => return None,
    };
    Some((buffer[..end].to_vec(), end + sep))
}

/// Stream an upstream SSE body through the filter. Errors after the response
/// head is written are logged and end the stream; they must not become a second
/// HTTP response on the same connection.
async fn pipe_sse<S>(stream: &mut S, response: reqwest::Response)
where
    S: AsyncWrite + Unpin,
{
    let mut filter = SseRedactedFilter::new();
    let mut buffer: Vec<u8> = Vec::new();
    let mut chunks = response.bytes_stream();
    while let Some(chunk) = chunks.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(e) => {
                tracing::debug!(error = %e, "thinking filter proxy SSE read failed");
                return;
            }
        };
        buffer.extend_from_slice(&chunk);
        while let Some((frame_bytes, consumed)) = split_sse_frame(&buffer) {
            buffer.drain(..consumed);
            let Ok(frame) = String::from_utf8(frame_bytes) else {
                tracing::debug!("thinking filter proxy dropped a non-UTF8 SSE frame");
                return;
            };
            if let Some(kept) = filter.filter_frame(&frame) {
                if write_frame(stream, &kept).await.is_err() {
                    return;
                }
            }
        }
        if buffer.len() > MAX_SSE_FRAME_BYTES {
            tracing::debug!("thinking filter proxy dropped an oversized SSE frame");
            return;
        }
    }
    if !buffer.is_empty() {
        if let Ok(frame) = String::from_utf8(buffer) {
            if let Some(kept) = filter.filter_frame(frame.trim_end()) {
                let _ = write_frame(stream, &kept).await;
            }
        }
    }
    tracing::debug!(removed = filter.removed_count(), "thinking proxy SSE done");
}

/// Write one filtered frame followed by the SSE frame terminator.
async fn write_frame<S>(stream: &mut S, frame: &str) -> Result<(), CoreError>
where
    S: AsyncWrite + Unpin,
{
    let mut out = Vec::with_capacity(frame.len() + 2);
    out.extend_from_slice(frame.as_bytes());
    out.extend_from_slice(b"\n\n");
    stream
        .write_all(&out)
        .await
        .map_err(model_err("proxy SSE write failed"))
}

async fn serve_connection(mut stream: TcpStream, client: Client, upstream_base: String) {
    if let Err(e) = handle_connection(&mut stream, &client, &upstream_base).await {
        tracing::debug!(error = %e, "thinking filter proxy request failed");
        let body = format!("thinking filter proxy: {e}");
        let head = format!(
            "HTTP/1.1 502 Bad Gateway\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(head.as_bytes()).await;
        let _ = stream.write_all(body.as_bytes()).await;
    }
}

async fn handle_connection<S>(
    stream: &mut S,
    client: &Client,
    upstream_base: &str,
) -> Result<(), CoreError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let (head, body) = read_request_head(stream).await?;
    let (method, target, headers) = parse_request_head(&head)?;
    let content_length = header_value(&headers, CONTENT_LENGTH.as_str())
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(0);
    if content_length > MAX_REQUEST_BODY_BYTES {
        return Err(CoreError::Config(format!(
            "proxy request body too large ({content_length} bytes)"
        )));
    }
    // An unfiltered chunked body would either be forwarded with the block still
    // in it or silently truncated; refuse instead.
    if headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    }) {
        return Err(CoreError::Config(
            "chunked request bodies are not supported by the thinking filter proxy".into(),
        ));
    }
    let mut body = read_body(stream, body, content_length).await?;

    let mut removed_request = 0;
    if !body.is_empty() {
        let content_type = header_value(&headers, CONTENT_TYPE.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.is_empty() || content_type.contains("json") {
            let mut value: Value =
                serde_json::from_slice(&body).map_err(model_err("invalid proxy request JSON"))?;
            removed_request = strip_redacted_from_messages_json(&mut value);
            if removed_request > 0 {
                body = serde_json::to_vec(&value)
                    .map_err(model_err("cannot serialize proxy request"))?;
            }
        }
    }

    let url = upstream_url(upstream_base, &target)?;
    let method =
        Method::from_bytes(method.as_bytes()).map_err(model_err("invalid proxy request method"))?;
    let mut request = client.request(method, &url);
    for (name, value) in &headers {
        // `accept-encoding` is dropped: a compressed response could not be
        // inspected, which would silently leak the block (fail open).
        if is_hop_by_hop(name)
            || name.eq_ignore_ascii_case(CONTENT_LENGTH.as_str())
            || name.eq_ignore_ascii_case(ACCEPT_ENCODING.as_str())
        {
            continue;
        }
        let name =
            HeaderName::from_bytes(name.as_bytes()).map_err(model_err("invalid proxy header"))?;
        let value =
            HeaderValue::from_str(value).map_err(model_err("invalid proxy header value"))?;
        request = request.header(name, value);
    }
    let response = request
        .header(ACCEPT_ENCODING, "identity")
        .body(body)
        .send()
        .await
        .map_err(model_err("upstream request failed"))?;
    let status = response.status();
    let response_headers = response.headers().clone();
    // Identity was requested; anything else means the body cannot be inspected,
    // so refuse rather than forward a block the adapter will choke on.
    if response_headers
        .get(CONTENT_ENCODING)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| !value.trim().is_empty() && !value.eq_ignore_ascii_case("identity"))
    {
        return Err(CoreError::Model(
            "upstream response is content-encoded; cannot filter redacted blocks".into(),
        ));
    }
    let content_type = response_headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    if content_type.contains("text/event-stream") {
        write_head(stream, status, &response_headers, None).await?;
        pipe_sse(stream, response).await;
        tracing::debug!(removed_request, "thinking filter proxy streamed a response");
        return Ok(());
    }

    let bytes = response
        .bytes()
        .await
        .map_err(model_err("upstream body failed"))?;
    let mut body = bytes.to_vec();
    let mut removed_response = 0;
    if content_type.contains("json") && !body.is_empty() {
        if let Ok(mut value) = serde_json::from_slice::<Value>(&body) {
            removed_response = strip_redacted_from_response_json(&mut value);
            if removed_response > 0 {
                body =
                    serde_json::to_vec(&value).map_err(model_err("cannot serialize response"))?;
            }
        }
    }
    write_response(stream, status, &response_headers, &body).await?;
    tracing::debug!(removed_request, removed_response, "proxy forwarded");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::task::{Context, Poll};
    use tokio::net::TcpListener as TokioListener;
    use tokio::sync::oneshot;

    const TEST_TIMEOUT: Duration = Duration::from_millis(50);

    #[test]
    fn filter_mode_accepts_enable_disable_and_defaults_on() {
        assert!(parse_filter_mode(None).expect("default"));
        assert!(parse_filter_mode(Some("")).expect("empty"));
        assert!(parse_filter_mode(Some("  ")).expect("blank"));
        assert!(parse_filter_mode(Some("enable")).expect("enable"));
        assert!(parse_filter_mode(Some(" ENABLE ")).expect("enable case"));
        assert!(!parse_filter_mode(Some("disable")).expect("disable"));
        assert!(!parse_filter_mode(Some("Disable")).expect("disable case"));
    }

    #[test]
    fn filter_mode_rejects_other_values() {
        for raw in ["1", "0", "true", "false", "yes", "on", "off", "enabled"] {
            let err = parse_filter_mode(Some(raw)).expect_err("must reject");
            assert!(err.to_string().contains("enable|disable"), "{err}");
            assert!(err.to_string().contains(raw), "{err}");
        }
    }

    #[test]
    fn request_filter_removes_only_redacted_blocks() {
        let mut body = json!({
            "model": "deepseek-v4",
            "system": [
                {"type": "text", "text": "be brief"},
                {"type": "redacted_thinking", "data": "sealed"}
            ],
            "messages": [
                {"role": "system", "content": "plain string"},
                {"role": "user", "content": [
                    {"type": "text", "text": "hello"},
                    {"type": "redacted_thinking", "data": "sealed"},
                    {"type": "thinking", "thinking": "reasoning", "signature": "sig"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "a"},
                    {"type": "redacted_thinking", "data": "b"},
                    {"type": "tool_use", "id": "t1", "name": "shell"}
                ]},
                {"role": "user"}
            ]
        });
        assert_eq!(strip_redacted_from_messages_json(&mut body), 4);
        let messages = body["messages"].as_array().expect("messages");
        assert_eq!(messages[0]["content"], json!("plain string"));
        let user = messages[1]["content"].as_array().expect("user content");
        assert_eq!(user.len(), 2);
        assert_eq!(user[0]["type"], "text");
        assert_eq!(user[1]["type"], "thinking");
        assert_eq!(user[1]["thinking"], "reasoning");
        let assistant = messages[2]["content"]
            .as_array()
            .expect("assistant content");
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "tool_use");
        assert!(messages[3].get("content").is_none());
        assert_eq!(
            body["system"].as_array().expect("system").len(),
            1,
            "system text must survive"
        );
        // Idempotent: a second pass changes nothing.
        assert_eq!(strip_redacted_from_messages_json(&mut body), 0);
    }

    #[test]
    fn request_filter_ignores_missing_or_malformed_shapes() {
        let mut body = json!({"messages": {"not": "an array"}});
        assert_eq!(strip_redacted_from_messages_json(&mut body), 0);
        let mut body = json!({});
        assert_eq!(strip_redacted_from_messages_json(&mut body), 0);
        let mut body = json!({"system": "plain string"});
        assert_eq!(strip_redacted_from_messages_json(&mut body), 0);
    }

    #[test]
    fn response_filter_handles_json_and_nested_shapes() {
        let mut body = json!({
            "content": [
                {"type": "text", "text": "x"},
                {"type": "redacted_thinking", "data": "sealed"}
            ]
        });
        assert_eq!(strip_redacted_from_response_json(&mut body), 1);
        assert_eq!(body["content"].as_array().expect("content").len(), 1);

        let mut body = json!({
            "message": {"content": [
                {"type": "redacted_thinking", "data": "sealed"},
                {"type": "tool_use", "id": "t"}
            ]}
        });
        assert_eq!(strip_redacted_from_response_json(&mut body), 1);
        assert_eq!(body["message"]["content"][0]["type"], "tool_use");
    }

    fn frame(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}")
    }

    #[test]
    fn sse_filter_drops_redacted_block_and_renumbers_later_indices() {
        let mut filter = SseRedactedFilter::new();
        let start_text = frame(
            "content_block_start",
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}),
        );
        let delta_text = frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
        );
        let start_redacted = frame(
            "content_block_start",
            json!({"type":"content_block_start","index":1,"content_block":{"type":"redacted_thinking","data":"sealed"}}),
        );
        let stop_redacted = frame(
            "content_block_stop",
            json!({"type":"content_block_stop","index":1}),
        );
        let start_tool = frame(
            "content_block_start",
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t","name":"shell"}}),
        );
        let delta_tool = frame(
            "content_block_delta",
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}),
        );
        let stop_tool = frame(
            "content_block_stop",
            json!({"type":"content_block_stop","index":2}),
        );
        let message_stop = frame("message_stop", json!({"type":"message_stop"}));

        assert_eq!(
            filter.filter_frame(&start_text).as_deref(),
            Some(start_text.as_str())
        );
        assert_eq!(
            filter.filter_frame(&delta_text).as_deref(),
            Some(delta_text.as_str())
        );
        assert_eq!(filter.filter_frame(&start_redacted), None);
        assert_eq!(filter.filter_frame(&stop_redacted), None);
        assert_eq!(filter.removed_count(), 1);

        let rewritten_tool = filter.filter_frame(&start_tool).expect("tool start kept");
        assert!(rewritten_tool.contains("\"index\":1"), "{rewritten_tool}");
        let rewritten_delta = filter.filter_frame(&delta_tool).expect("tool delta kept");
        assert!(rewritten_delta.contains("\"index\":1"), "{rewritten_delta}");
        let rewritten_stop = filter.filter_frame(&stop_tool).expect("tool stop kept");
        assert!(rewritten_stop.contains("\"index\":1"), "{rewritten_stop}");
        assert_eq!(
            filter.filter_frame(&message_stop).as_deref(),
            Some(message_stop.as_str())
        );
    }

    #[test]
    fn sse_filter_handles_multiple_redacted_blocks_and_message_start() {
        let mut filter = SseRedactedFilter::new();
        let message_start = frame(
            "message_start",
            json!({"type":"message_start","message":{"content":[
                {"type":"redacted_thinking","data":"sealed"},
                {"type":"text","text":""}
            ]}}),
        );
        let rewritten = filter.filter_frame(&message_start).expect("message start");
        assert!(!rewritten.contains("redacted_thinking"), "{rewritten}");

        for index in [0, 2] {
            let start = frame(
                "content_block_start",
                json!({"type":"content_block_start","index":index,"content_block":{"type":"redacted_thinking","data":"s"}}),
            );
            assert_eq!(filter.filter_frame(&start), None);
        }
        assert_eq!(filter.removed_count(), 2);
        let other = frame(
            "content_block_start",
            json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}),
        );
        let rewritten = filter.filter_frame(&other).expect("kept");
        assert!(rewritten.contains("\"index\":1"), "{rewritten}");
    }

    #[test]
    fn sse_filter_passes_through_unknown_and_malformed_frames() {
        let mut filter = SseRedactedFilter::new();
        for frame in [
            ": ping".to_string(),
            "event: ping\ndata: not-json".to_string(),
            frame("unknown", json!({"type":"unknown","index":0})),
            frame(
                "content_block_start",
                json!({"type":"content_block_start","content_block":{"type":"text"}}),
            ),
        ] {
            assert_eq!(filter.filter_frame(&frame).as_deref(), Some(frame.as_str()));
        }
        assert_eq!(filter.removed_count(), 0);
    }

    #[test]
    fn sse_filter_passes_through_frames_without_index_or_redaction() {
        let mut filter = SseRedactedFilter::new();
        for frame in [
            frame(
                "content_block_start",
                json!({"type":"content_block_start","content_block":{"type":"text","text":""}}),
            ),
            frame(
                "content_block_delta",
                json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"x"}}),
            ),
            frame("content_block_stop", json!({"type":"content_block_stop"})),
            // `message_start` without a content array, and with nothing removed.
            frame("message_start", json!({"type":"message_start"})),
            frame(
                "message_start",
                json!({"type":"message_start","message":{"content":[{"type":"text","text":""}]}}),
            ),
        ] {
            assert_eq!(filter.filter_frame(&frame).as_deref(), Some(frame.as_str()));
        }
        assert_eq!(filter.removed_count(), 0);
    }

    #[test]
    fn sse_frame_split_prefers_the_earliest_terminator() {
        assert_eq!(split_sse_frame(b"a\r\n\r\nb"), Some((b"a".to_vec(), 5)));
        assert_eq!(split_sse_frame(b"a\n\nb"), Some((b"a".to_vec(), 3)));
        assert_eq!(split_sse_frame(b"a\r\n\r\nb\n\n"), Some((b"a".to_vec(), 5)));
        assert_eq!(split_sse_frame(b"a\n\nb\r\n\r\n"), Some((b"a".to_vec(), 3)));
        assert_eq!(split_sse_frame(b"a\nb"), None);
    }

    #[test]
    fn upstream_url_handles_root_and_bare_paths() {
        let base = dsh_base_url("https://gateway.example/");
        assert_eq!(base, "https://gateway.example/anthropic/v1");
        assert_eq!(
            upstream_url(&base, "/anthropic/v1").expect("root"),
            "https://gateway.example/anthropic/v1"
        );
        assert_eq!(
            upstream_url(&base, "/messages?stream=true").expect("query"),
            "https://gateway.example/anthropic/v1/messages?stream=true"
        );
        assert!(upstream_url(&base, "messages").is_err());
    }

    #[test]
    fn response_head_drops_hop_by_hop_and_opaque_headers() {
        let mut headers = HeaderMap::new();
        headers.insert("connection", HeaderValue::from_static("keep-alive"));
        headers.insert("content-encoding", HeaderValue::from_static("gzip"));
        headers.insert("content-length", HeaderValue::from_static("99"));
        headers.insert("x-request-id", HeaderValue::from_static("abc"));
        // obs-text: accepted by `from_bytes`, rejected by `to_str`.
        headers.insert(
            "x-opaque",
            HeaderValue::from_bytes(b"\x80").expect("opaque value"),
        );
        let head = build_head(StatusCode::OK, &headers, Some(2));
        assert!(head.contains("x-request-id: abc"), "{head}");
        assert!(head.contains("Content-Length: 2"), "{head}");
        assert!(!head.contains("connection"), "{head}");
        assert!(!head.contains("content-encoding"), "{head}");
        assert!(!head.contains("x-opaque"), "{head}");
        assert!(!head.contains("99"), "{head}");

        // Unknown status codes have no canonical reason phrase.
        let custom = StatusCode::from_u16(599).expect("status");
        let head = build_head(custom, &HeaderMap::new(), None);
        assert!(head.starts_with("HTTP/1.1 599 "), "{head}");
        assert!(!head.contains("Content-Length"), "{head}");
    }

    #[derive(Debug)]
    struct CapturedRequest {
        target: String,
        body: String,
        x_api_key: String,
        authorization: String,
        accept_encoding: String,
        connection: String,
        keep_alive: String,
    }

    enum UpstreamReply {
        Json(String),
        Sse(Vec<u8>),
        EncodedJson(String),
        Plain(String),
        TruncatedSse,
    }

    async fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let (head, mut body) = read_request_head(stream).await.expect("request head");
        let (_, _, headers) = parse_request_head(&head).expect("request line");
        let len = header_value(&headers, "content-length")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        let mut rest = vec![0u8; len.saturating_sub(body.len())];
        stream.read_exact(&mut rest).await.expect("body read");
        body.extend_from_slice(&rest);
        (head, body)
    }

    async fn write_http(
        stream: &mut TcpStream,
        content_type: &str,
        extra: Option<(&str, &str)>,
        body: &[u8],
    ) {
        let mut head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n",
            body.len()
        );
        if let Some((name, value)) = extra {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes()).await.expect("write head");
        stream.write_all(body).await.expect("write body");
    }

    /// One-shot fake Anthropic-compatible gateway: records what the proxy sent,
    /// then answers with a body that contains a redacted block.
    async fn spawn_upstream(
        reply: UpstreamReply,
    ) -> (SocketAddr, oneshot::Receiver<CapturedRequest>) {
        let listener = TokioListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = oneshot::channel();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept");
            let (head, body) = read_http_request(&mut stream).await;
            let (_, target, headers) = parse_request_head(&head).expect("parse");
            let header = |name: &str| header_value(&headers, name).unwrap_or("").to_string();
            let _ = tx.send(CapturedRequest {
                target,
                body: String::from_utf8_lossy(&body).into_owned(),
                x_api_key: header("x-api-key"),
                authorization: header("authorization"),
                accept_encoding: header("accept-encoding"),
                connection: header("connection"),
                keep_alive: header("keep-alive"),
            });
            match reply {
                UpstreamReply::Json(body) => {
                    write_http(&mut stream, "application/json", None, body.as_bytes()).await;
                }
                UpstreamReply::EncodedJson(body) => {
                    write_http(
                        &mut stream,
                        "application/json",
                        Some(("Content-Encoding", "gzip")),
                        body.as_bytes(),
                    )
                    .await;
                }
                UpstreamReply::Plain(body) => {
                    write_http(&mut stream, "text/plain", None, body.as_bytes()).await;
                }
                UpstreamReply::Sse(body) => {
                    write_http(&mut stream, "text/event-stream", None, &body).await;
                }
                UpstreamReply::TruncatedSse => {
                    // Declare more bytes than are sent, then close: the proxy's
                    // body stream fails with an incomplete-message error.
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 4096\r\n\r\n",
                        )
                        .await;
                    let _ = stream
                        .write_all(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n")
                        .await;
                }
            }
        });
        (addr, rx)
    }

    async fn start_proxy(upstream: SocketAddr) -> ThinkingFilterProxy {
        ThinkingFilterProxy::start(&format!("http://{upstream}"))
            .await
            .expect("proxy start")
    }

    fn proxy_addr(proxy: &ThinkingFilterProxy) -> SocketAddr {
        proxy
            .local_base_url()
            .trim_start_matches("http://")
            .parse()
            .expect("proxy addr")
    }

    fn post_request(target: &str, body: &str) -> String {
        format!(
            "POST {target} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nX-Api-Key: secret-key\r\nAuthorization: Bearer gw-key\r\nAccept-Encoding: gzip, deflate\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    async fn exchange(proxy: &ThinkingFilterProxy, request: &str) -> String {
        let mut stream = TcpStream::connect(proxy_addr(proxy))
            .await
            .expect("connect proxy");
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        String::from_utf8_lossy(&response).into_owned()
    }

    /// Drive `handle_connection` over an in-memory socket pair: no ports, no
    /// timing, and the request is closed after the fixed payload.
    async fn run_connection(request: &[u8], upstream: SocketAddr) -> Result<String, CoreError> {
        let (mut client, mut server) = tokio::io::duplex(512 * 1024);
        client.write_all(request).await.expect("write request");
        client.shutdown().await.expect("shutdown");
        let base = dsh_base_url(&format!("http://{upstream}"));
        let http = build_client().expect("client");
        handle_connection(&mut server, &http, &base).await?;
        drop(server);
        let mut response = String::new();
        client.read_to_string(&mut response).await.expect("read");
        Ok(response)
    }

    async fn idle_upstream() -> SocketAddr {
        TokioListener::bind("127.0.0.1:0")
            .await
            .expect("bind")
            .local_addr()
            .expect("addr")
    }

    #[tokio::test]
    async fn proxy_filters_json_request_and_response() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::Json(
            json!({"content":[
                {"type":"redacted_thinking","data":"sealed"},
                {"type":"text","text":"hello"}
            ]})
            .to_string(),
        ))
        .await;
        let proxy = start_proxy(upstream).await;
        let body = json!({
            "model": "m",
            "messages": [{"role":"user","content":[
                {"type":"redacted_thinking","data":"old"},
                {"type":"text","text":"hi"}
            ]}]
        })
        .to_string();
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", &body)).await;

        let captured = captured.await.expect("captured request");
        assert_eq!(captured.target, "/anthropic/v1/messages");
        assert_eq!(captured.x_api_key, "secret-key");
        assert_eq!(captured.authorization, "Bearer gw-key");
        // Compression is refused so every body can be inspected.
        assert_eq!(captured.accept_encoding, "identity");
        // Hop-by-hop headers never reach the gateway.
        assert!(captured.connection.is_empty(), "{captured:?}");
        assert!(captured.keep_alive.is_empty(), "{captured:?}");
        assert!(
            !captured.body.contains("redacted_thinking"),
            "{}",
            captured.body
        );
        assert!(
            captured.body.contains("\"text\":\"hi\""),
            "{}",
            captured.body
        );

        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("redacted_thinking"), "{response}");
        assert!(response.contains("\"text\":\"hello\""), "{response}");
    }

    #[tokio::test]
    async fn proxy_filters_sse_stream_and_renumbers_indices() {
        let sse = concat!(
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"sealed\"}}\n\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"t\",\"name\":\"shell\"}}\n\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let (upstream, captured) =
            spawn_upstream(UpstreamReply::Sse(sse.as_bytes().to_vec())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("redacted_thinking"), "{response}");
        assert!(response.contains("\"index\":1"), "{response}");
        assert!(response.contains("tool_use"), "{response}");
        assert!(response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_handles_crlf_sse_frames() {
        let sse = "event: content_block_start\r\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\r\n\r\nevent: message_stop\r\ndata: {\"type\":\"message_stop\"}\r\n\r\n";
        let (upstream, captured) =
            spawn_upstream(UpstreamReply::Sse(sse.as_bytes().to_vec())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.contains("content_block_start"), "{response}");
        assert!(response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_flushes_a_trailing_sse_frame_without_terminator() {
        let sse = "event: message_stop\ndata: {\"type\":\"message_stop\"}";
        let (upstream, captured) =
            spawn_upstream(UpstreamReply::Sse(sse.as_bytes().to_vec())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_drops_a_non_utf8_sse_frame() {
        let mut sse = Vec::new();
        sse.extend_from_slice(b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n");
        sse.extend_from_slice(b"event: bad\ndata: \xff\xfe\n\n");
        sse.extend_from_slice(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        let (upstream, captured) = spawn_upstream(UpstreamReply::Sse(sse)).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.contains("message_start"), "{response}");
        assert!(!response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_drops_an_oversized_sse_frame() {
        let mut sse = Vec::new();
        sse.extend_from_slice(
            b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"",
        );
        sse.extend(std::iter::repeat_n(b'a', MAX_SSE_FRAME_BYTES + 16));
        let (upstream, captured) = spawn_upstream(UpstreamReply::Sse(sse)).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("aaaa"), "{response}");
    }

    #[tokio::test]
    async fn proxy_stops_when_the_upstream_stream_is_truncated() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::TruncatedSse).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_passes_through_non_json_responses() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::Plain("plain text".into())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("plain text"), "{response}");
    }

    #[tokio::test]
    async fn proxy_forwards_an_unparsable_json_response_untouched() {
        // `application/json` with a body serde cannot read must be forwarded
        // as-is: refusing would break a gateway quirk, and the body never had
        // a redacted block to leak.
        let (upstream, captured) = spawn_upstream(UpstreamReply::Json("not json".into())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.ends_with("not json"), "{response}");
    }

    #[tokio::test]
    async fn proxy_forwards_a_non_json_request_body_untouched() {
        // Only JSON bodies can hide a content block; anything else goes
        // through byte for byte (and is still recorded upstream).
        let (upstream, captured) = spawn_upstream(UpstreamReply::Plain("ok".into())).await;
        let proxy = start_proxy(upstream).await;
        let request = "POST /anthropic/v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let response = exchange(&proxy, request).await;

        let captured = captured.await.expect("captured request");
        assert_eq!(captured.body, "hello");
        assert!(response.ends_with("ok"), "{response}");
    }

    #[tokio::test]
    async fn proxy_drops_a_trailing_sse_frame_the_filter_suppresses() {
        // A stream that ends mid-frame cannot be parsed; the filter returns
        // None and the partial bytes are dropped rather than forwarded.
        let sse = b"event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"redacted_thinking\"}}";
        let (upstream, captured) = spawn_upstream(UpstreamReply::Sse(sse.to_vec())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("redacted_thinking"), "{response}");
    }

    #[tokio::test]
    async fn proxy_drops_a_trailing_sse_frame_that_is_not_utf8() {
        let mut sse = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n".to_vec();
        sse.extend_from_slice(b"event: bad\ndata: \xff\xfe");
        let (upstream, captured) = spawn_upstream(UpstreamReply::Sse(sse)).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.contains("message_start"), "{response}");
        assert!(!response.contains("event: bad"), "{response}");
    }

    #[test]
    fn an_empty_sse_line_is_neither_a_prefix_nor_data() {
        let (prefix, data) =
            parse_sse_frame("event: message_start\n\ndata: {\"type\":\"message_start\"}")
                .expect("frame");
        assert_eq!(prefix, vec!["event: message_start".to_string()]);
        assert_eq!(data["type"], "message_start");
    }

    #[tokio::test]
    async fn proxy_rejects_a_content_encoded_upstream() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::EncodedJson("{}".into())).await;
        let proxy = start_proxy(upstream).await;
        let response = exchange(&proxy, &post_request("/anthropic/v1/messages", "{}")).await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("content-encoded"), "{response}");
    }

    #[tokio::test]
    async fn proxy_rejects_chunked_request_bodies() {
        let upstream = idle_upstream().await;
        let proxy = start_proxy(upstream).await;
        let request = "POST /anthropic/v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        let response = exchange(&proxy, request).await;
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("chunked request bodies"), "{response}");
    }

    #[tokio::test]
    async fn proxy_serves_a_request_over_the_in_memory_socket() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::Json(
            json!({"content":[{"type":"text","text":"ok"}]}).to_string(),
        ))
        .await;
        let response = run_connection(
            post_request("/anthropic/v1/messages", "{}").as_bytes(),
            upstream,
        )
        .await
        .expect("handled");
        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(response.contains("\"text\":\"ok\""), "{response}");
    }

    #[tokio::test]
    async fn proxy_rejects_an_oversized_body_declaration() {
        let upstream = idle_upstream().await;
        let request = format!(
            "POST /anthropic/v1/messages HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
            MAX_REQUEST_BODY_BYTES + 1
        );
        let err = run_connection(request.as_bytes(), upstream)
            .await
            .expect_err("body too large");
        assert!(err.to_string().contains("body too large"), "{err}");
    }

    #[tokio::test]
    async fn proxy_rejects_an_incomplete_body() {
        let upstream = idle_upstream().await;
        let request = "POST /anthropic/v1/messages HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 10\r\n\r\nabc";
        let err = run_connection(request.as_bytes(), upstream)
            .await
            .expect_err("incomplete body");
        assert!(
            err.to_string().contains("incomplete proxy request body"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn proxy_rejects_invalid_json_bodies() {
        let upstream = idle_upstream().await;
        let request =
            "POST /anthropic/v1/messages HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 8\r\n\r\nnot-json";
        let err = run_connection(request.as_bytes(), upstream)
            .await
            .expect_err("invalid json");
        assert!(
            err.to_string().contains("invalid proxy request JSON"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn proxy_rejects_unparsable_request_lines_and_headers() {
        let upstream = idle_upstream().await;
        let cases = [
            ("\r\n\r\n", "missing request method"),
            ("  \t\r\n\r\n", "missing request method"),
            ("POST\r\n\r\n", "missing request target"),
            ("G@T /x HTTP/1.1\r\n\r\n", "invalid proxy request method"),
            (
                "POST /x HTTP/1.1\r\nBad Name: value\r\n\r\n",
                "invalid proxy header",
            ),
            (
                "POST /x HTTP/1.1\r\nX-Bad: \u{7f}\r\n\r\n",
                "invalid proxy header value",
            ),
        ];
        for (request, expected) in cases {
            let err = run_connection(request.as_bytes(), upstream)
                .await
                .expect_err(expected);
            assert!(err.to_string().contains(expected), "{request:?}: {err}");
        }
    }

    #[tokio::test]
    async fn proxy_reports_an_unreachable_upstream() {
        // Bind and drop to get a port that refuses connections.
        let upstream = idle_upstream().await;
        let request = post_request("/anthropic/v1/messages", "{}");
        let err = run_connection(request.as_bytes(), upstream)
            .await
            .expect_err("upstream failure");
        assert!(err.to_string().contains("upstream request failed"), "{err}");
    }

    #[tokio::test]
    async fn proxy_reads_a_body_that_arrives_after_the_head() {
        let (upstream, captured) = spawn_upstream(UpstreamReply::Json("{}".into())).await;
        let base = dsh_base_url(&format!("http://{upstream}"));
        let http = build_client().expect("client");
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let task = tokio::spawn(async move { handle_connection(&mut server, &http, &base).await });
        client
            .write_all(
                b"POST /anthropic/v1/messages HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 7\r\n\r\n",
            )
            .await
            .expect("head");
        // Give the proxy a chance to consume the head before the body exists.
        tokio::time::sleep(TEST_TIMEOUT).await;
        client.write_all(b"{\"a\":1}").await.expect("body");
        client.shutdown().await.expect("shutdown");
        task.await.expect("join").expect("handled");
        assert_eq!(captured.await.expect("captured").body, "{\"a\":1}");
    }

    #[tokio::test]
    async fn proxy_fails_closed_on_empty_upstream() {
        let err = ThinkingFilterProxy::start("")
            .await
            .expect_err("empty upstream");
        assert!(err.to_string().contains("empty upstream"), "{err}");
    }

    #[tokio::test]
    async fn start_on_binds_the_requested_address() {
        // Port 0 lets the OS pick, but the bind address (the part a containerised
        // harness needs) is the caller's.
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let proxy = ThinkingFilterProxy::start_on("https://gateway.example", addr)
            .await
            .expect("proxy");
        assert!(
            proxy.local_base_url().starts_with("http://127.0.0.1:"),
            "{}",
            proxy.local_base_url()
        );
    }

    #[tokio::test]
    async fn start_on_reports_a_bind_failure() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = listener.local_addr().unwrap();
        let err = ThinkingFilterProxy::start_on("https://gateway.example", taken)
            .await
            .expect_err("address in use");
        assert!(
            err.to_string()
                .contains("cannot bind thinking filter proxy"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn start_reports_a_bind_failure() {
        let err = ThinkingFilterProxy::start_with("https://gateway.example", || async {
            Err::<AcceptFailingListener, _>(io::Error::other("address in use"))
        })
        .await
        .expect_err("bind failure");
        assert!(
            err.to_string()
                .contains("cannot bind thinking filter proxy"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn start_reports_an_address_failure() {
        let err = ThinkingFilterProxy::start_with("https://gateway.example", || async {
            Ok(AddressFailingListener)
        })
        .await
        .expect_err("address failure");
        assert!(
            err.to_string().contains("cannot read proxy address"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn accept_failures_end_the_accept_loop() {
        let proxy = ThinkingFilterProxy::start_with("https://gateway.example", || async {
            Ok(AcceptFailingListener)
        })
        .await
        .expect("proxy");
        assert_eq!(proxy.local_base_url(), "http://127.0.0.1:9");
        // Let the spawned loop observe the failure and break out.
        tokio::time::sleep(TEST_TIMEOUT).await;
    }

    #[test]
    fn client_build_failure_is_a_config_error() {
        let err = build_client_with(|| Err::<Client, _>(io::Error::other("no tls backend")))
            .expect_err("client failure");
        assert!(
            err.to_string().contains("cannot build proxy TLS client"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn request_head_must_stay_bounded() {
        let (mut writer, mut reader) = tokio::io::duplex(256 * 1024);
        let oversized = vec![b'X'; MAX_REQUEST_HEAD_BYTES + 4096];
        writer.write_all(&oversized).await.expect("write head");
        let err = read_request_head(&mut reader)
            .await
            .expect_err("head too large");
        assert!(err.to_string().contains("head too large"), "{err}");
    }

    #[tokio::test]
    async fn request_head_eof_is_reported() {
        let (writer, mut reader) = tokio::io::duplex(64);
        drop(writer);
        let err = read_request_head(&mut reader).await.expect_err("eof");
        assert!(
            err.to_string().contains("closed before request head"),
            "{err}"
        );
    }

    /// Reads fail immediately: the `SO_LINGER 0` reset cannot be relied on to
    /// land before the proxy's next read on every kernel, so the seam is a
    /// reader that always errors.
    struct FailingReader;

    impl AsyncRead for FailingReader {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut tokio::io::ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::ConnectionReset, "reset")))
        }
    }

    #[tokio::test]
    async fn body_bytes_after_the_head_are_appended() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"hello").await.expect("write body");
        let body = read_body(&mut reader, Vec::new(), 5).await.expect("body");
        assert_eq!(body, b"hello");
        // Bytes that arrived with the head are kept and topped up.
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"lo").await.expect("write body");
        let body = read_body(&mut reader, b"hel".to_vec(), 5)
            .await
            .expect("body");
        assert_eq!(body, b"hello");
    }

    #[tokio::test]
    async fn a_short_body_is_an_error() {
        let (mut writer, mut reader) = tokio::io::duplex(64);
        writer.write_all(b"abc").await.expect("write body");
        writer.shutdown().await.expect("shutdown");
        let err = read_body(&mut reader, Vec::new(), 5)
            .await
            .expect_err("short body");
        assert!(
            err.to_string().contains("incomplete proxy request body"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn request_read_failure_is_reported() {
        let err = read_request_head(&mut FailingReader)
            .await
            .expect_err("read failure");
        assert!(err.to_string().contains("proxy read failed"), "{err}");
    }

    /// Accepts anything but fails every write, so the three write branches are
    /// exercised without a peer that has to disappear at the right moment.
    struct FailingWriter;

    impl AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed")))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn sse_write_failure_ends_the_stream() {
        let (upstream, _captured) = spawn_upstream(UpstreamReply::Sse(
            b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n".to_vec(),
        ))
        .await;
        let http = build_client().expect("client");
        let response = http
            .get(format!("http://{upstream}/anthropic/v1/messages"))
            .send()
            .await
            .expect("response");
        // The filtered frame cannot be written: the stream must end quietly.
        pipe_sse(&mut FailingWriter, response).await;
    }

    #[tokio::test]
    async fn write_failures_are_reported() {
        let headers = HeaderMap::new();
        let err = write_head(&mut FailingWriter, StatusCode::OK, &headers, None)
            .await
            .expect_err("head write");
        assert!(err.to_string().contains("proxy write failed"), "{err}");

        let err = write_response(&mut FailingWriter, StatusCode::OK, &headers, b"x")
            .await
            .expect_err("response write");
        assert!(
            err.to_string().contains("proxy response write failed"),
            "{err}"
        );

        let err = write_frame(&mut FailingWriter, "data")
            .await
            .expect_err("frame write");
        assert!(err.to_string().contains("proxy SSE write failed"), "{err}");

        // Flushing and shutting down still succeed; only writes fail.
        let mut writer = FailingWriter;
        writer.flush().await.expect("flush");
        writer.shutdown().await.expect("shutdown");
    }

    struct AddressFailingListener;

    #[async_trait::async_trait]
    impl ProxyListener for AddressFailingListener {
        async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
            Err(io::Error::other("no address"))
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Err(io::Error::other("no address"))
        }
    }

    #[tokio::test]
    async fn a_listener_without_an_address_fails_every_call() {
        let err = AddressFailingListener.local_addr().expect_err("no address");
        assert!(err.to_string().contains("no address"), "{err}");
        let err = AddressFailingListener
            .accept()
            .await
            .expect_err("no address");
        assert!(err.to_string().contains("no address"), "{err}");
    }

    struct AcceptFailingListener;

    #[async_trait::async_trait]
    impl ProxyListener for AcceptFailingListener {
        async fn accept(&self) -> io::Result<(TcpStream, SocketAddr)> {
            Err(io::Error::other("accept failed"))
        }

        fn local_addr(&self) -> io::Result<SocketAddr> {
            Ok("127.0.0.1:9".parse::<SocketAddr>().expect("loopback addr"))
        }
    }
}
