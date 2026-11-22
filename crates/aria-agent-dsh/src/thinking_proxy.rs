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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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
    pub fn filter_frame(&mut self, frame: &str) -> Result<Option<String>, CoreError> {
        let Some((prefix, mut data)) = parse_sse_frame(frame) else {
            return Ok(Some(frame.to_string()));
        };
        let kind = data.get("type").and_then(Value::as_str).unwrap_or("");
        match kind {
            "content_block_start" => {
                let Some(index) = data.get("index").and_then(Value::as_u64) else {
                    return Ok(Some(frame.to_string()));
                };
                if data.get("content_block").is_some_and(is_redacted) {
                    self.suppressed.insert(index);
                    return Ok(None);
                }
                let new_index = remap_index(index, &self.suppressed);
                if new_index == index {
                    return Ok(Some(frame.to_string()));
                }
                data["index"] = json!(new_index);
            }
            "content_block_delta" | "content_block_stop" => {
                let Some(index) = data.get("index").and_then(Value::as_u64) else {
                    return Ok(Some(frame.to_string()));
                };
                if self.suppressed.contains(&index) {
                    return Ok(None);
                }
                let new_index = remap_index(index, &self.suppressed);
                if new_index == index {
                    return Ok(Some(frame.to_string()));
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
                    return Ok(Some(frame.to_string()));
                }
            }
            _ => return Ok(Some(frame.to_string())),
        }
        Ok(Some(rebuild_frame(&prefix, &data)))
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
        let upstream_base = dsh_base_url(upstream_root);
        if upstream_base.is_empty() {
            return Err(CoreError::Config(
                "thinking filter proxy: empty upstream root".into(),
            ));
        }
        // `rustls-tls` uses the compiled-in webpki-roots 1.0.10+ store, never the
        // system CA bundle (eMudhra/emSign CA compromise, 2026-10).
        let client = Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .read_timeout(READ_TIMEOUT)
            .build()
            .map_err(|e| CoreError::Config(format!("cannot build proxy TLS client: {e}")))?;
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(|e| CoreError::Config(format!("cannot bind thinking filter proxy: {e}")))?;
        let addr = listener
            .local_addr()
            .map_err(|e| CoreError::Config(format!("cannot read proxy address: {e}")))?;
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

async fn read_request_head(stream: &mut TcpStream) -> Result<(String, Vec<u8>), CoreError> {
    let mut buf = Vec::new();
    loop {
        if buf.len() > MAX_REQUEST_HEAD_BYTES {
            return Err(CoreError::Config("proxy request head too large".into()));
        }
        let mut chunk = [0u8; 4096];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| CoreError::Model(format!("proxy read failed: {e}")))?;
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
    let request_line = lines
        .next()
        .ok_or_else(|| CoreError::Model("empty proxy request".into()))?;
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

async fn write_response_head(
    stream: &mut TcpStream,
    status: StatusCode,
    headers: &HeaderMap,
    content_length: Option<usize>,
) -> Result<(), CoreError> {
    let mut head = format!(
        "HTTP/1.1 {} {}\r\n",
        status.as_u16(),
        status.canonical_reason().unwrap_or("")
    );
    for (name, value) in headers {
        // `content-length` is recomputed, `content-encoding` is never forwarded
        // (the proxy forces identity encoding so it can always inspect bodies).
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
    stream
        .write_all(head.as_bytes())
        .await
        .map_err(|e| CoreError::Model(format!("proxy write failed: {e}")))
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
async fn pipe_sse(stream: &mut TcpStream, response: reqwest::Response) {
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
            let frame = match String::from_utf8(frame_bytes) {
                Ok(frame) => frame,
                Err(_) => {
                    tracing::debug!("thinking filter proxy dropped a non-UTF8 SSE frame");
                    return;
                }
            };
            match filter.filter_frame(&frame) {
                Ok(Some(kept)) => {
                    if stream.write_all(kept.as_bytes()).await.is_err()
                        || stream.write_all(b"\n\n").await.is_err()
                    {
                        return;
                    }
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::debug!(error = %e, "thinking filter proxy SSE filter failed");
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
            if let Ok(Some(kept)) = filter.filter_frame(frame.trim_end()) {
                let _ = stream.write_all(kept.as_bytes()).await;
                let _ = stream.write_all(b"\n\n").await;
            }
        }
    }
    tracing::debug!(
        removed = filter.removed_count(),
        "thinking filter proxy finished SSE stream"
    );
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

async fn handle_connection(
    stream: &mut TcpStream,
    client: &Client,
    upstream_base: &str,
) -> Result<(), CoreError> {
    let (head, mut body) = read_request_head(stream).await?;
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
    while body.len() < content_length {
        let mut chunk = [0u8; 8192];
        let n = stream
            .read(&mut chunk)
            .await
            .map_err(|e| CoreError::Model(format!("proxy body read failed: {e}")))?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    if body.len() != content_length {
        return Err(CoreError::Model("incomplete proxy request body".into()));
    }

    let mut removed_request = 0;
    if !body.is_empty() {
        let content_type = header_value(&headers, CONTENT_TYPE.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if content_type.is_empty() || content_type.contains("json") {
            let mut value: Value = serde_json::from_slice(&body)
                .map_err(|e| CoreError::Model(format!("invalid proxy request JSON: {e}")))?;
            removed_request = strip_redacted_from_messages_json(&mut value);
            if removed_request > 0 {
                body = serde_json::to_vec(&value).map_err(|e| {
                    CoreError::Model(format!("cannot serialize proxy request: {e}"))
                })?;
            }
        }
    }

    let url = upstream_url(upstream_base, &target)?;
    let method = Method::from_bytes(method.as_bytes())
        .map_err(|e| CoreError::Model(format!("invalid proxy request method: {e}")))?;
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
        let name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|e| CoreError::Model(format!("invalid proxy header: {e}")))?;
        let value = HeaderValue::from_str(value)
            .map_err(|e| CoreError::Model(format!("invalid proxy header value: {e}")))?;
        request = request.header(name, value);
    }
    let response = request
        .header(ACCEPT_ENCODING, "identity")
        .body(body)
        .send()
        .await
        .map_err(|e| CoreError::Model(format!("upstream request failed: {e}")))?;
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
        write_response_head(stream, status, &response_headers, None).await?;
        pipe_sse(stream, response).await;
        tracing::debug!(removed_request, "thinking filter proxy streamed a response");
        return Ok(());
    }

    let bytes = response
        .bytes()
        .await
        .map_err(|e| CoreError::Model(format!("upstream body failed: {e}")))?;
    let mut body = bytes.to_vec();
    let mut removed_response = 0;
    if content_type.contains("json") && !body.is_empty() {
        if let Ok(mut value) = serde_json::from_slice::<Value>(&body) {
            removed_response = strip_redacted_from_response_json(&mut value);
            if removed_response > 0 {
                body = serde_json::to_vec(&value)
                    .map_err(|e| CoreError::Model(format!("cannot serialize response: {e}")))?;
            }
        }
    }
    write_response_head(stream, status, &response_headers, Some(body.len())).await?;
    stream
        .write_all(&body)
        .await
        .map_err(|e| CoreError::Model(format!("proxy response write failed: {e}")))?;
    tracing::debug!(
        removed_request,
        removed_response,
        "thinking filter proxy forwarded a response"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
            filter.filter_frame(&start_text).unwrap().as_deref(),
            Some(start_text.as_str())
        );
        assert_eq!(
            filter.filter_frame(&delta_text).unwrap().as_deref(),
            Some(delta_text.as_str())
        );
        assert_eq!(filter.filter_frame(&start_redacted).unwrap(), None);
        assert_eq!(filter.filter_frame(&stop_redacted).unwrap(), None);
        assert_eq!(filter.removed_count(), 1);

        let rewritten_tool = filter
            .filter_frame(&start_tool)
            .unwrap()
            .expect("tool start kept");
        assert!(rewritten_tool.contains("\"index\":1"), "{rewritten_tool}");
        let rewritten_delta = filter
            .filter_frame(&delta_tool)
            .unwrap()
            .expect("tool delta kept");
        assert!(rewritten_delta.contains("\"index\":1"), "{rewritten_delta}");
        let rewritten_stop = filter
            .filter_frame(&stop_tool)
            .unwrap()
            .expect("tool stop kept");
        assert!(rewritten_stop.contains("\"index\":1"), "{rewritten_stop}");
        assert_eq!(
            filter.filter_frame(&message_stop).unwrap().as_deref(),
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
        let rewritten = filter
            .filter_frame(&message_start)
            .unwrap()
            .expect("message start");
        assert!(!rewritten.contains("redacted_thinking"), "{rewritten}");

        for index in [0, 2] {
            let start = frame(
                "content_block_start",
                json!({"type":"content_block_start","index":index,"content_block":{"type":"redacted_thinking","data":"s"}}),
            );
            assert_eq!(filter.filter_frame(&start).unwrap(), None);
        }
        assert_eq!(filter.removed_count(), 2);
        let other = frame(
            "content_block_start",
            json!({"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}),
        );
        let rewritten = filter.filter_frame(&other).unwrap().expect("kept");
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
            assert_eq!(
                filter.filter_frame(&frame).unwrap().as_deref(),
                Some(frame.as_str())
            );
        }
        assert_eq!(filter.removed_count(), 0);
    }

    #[derive(Debug)]
    struct CapturedRequest {
        target: String,
        body: String,
        x_api_key: String,
        authorization: String,
        accept_encoding: String,
    }

    async fn read_http_request(stream: &mut TcpStream) -> (String, Vec<u8>) {
        let (head, mut body) = read_request_head(stream).await.expect("request head");
        let (_, _, headers) = parse_request_head(&head).expect("request line");
        let len = header_value(&headers, "content-length")
            .and_then(|value| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        while body.len() < len {
            let mut chunk = [0u8; 4096];
            let n = stream.read(&mut chunk).await.expect("body read");
            if n == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..n]);
        }
        (head, body)
    }

    /// One-shot fake Anthropic-compatible gateway. Records what the proxy sent
    /// and replies with a response containing a redacted block.
    async fn spawn_fake_upstream(
        sse: bool,
    ) -> (SocketAddr, tokio::sync::oneshot::Receiver<CapturedRequest>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let addr = listener.local_addr().expect("addr");
        let (tx, rx) = tokio::sync::oneshot::channel();
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
            });
            let (content_type, response_body) = if sse {
                (
                    "text/event-stream",
                    concat!(
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
                    )
                    .to_string(),
                )
            } else {
                (
                    "application/json",
                    json!({"content":[
                        {"type":"redacted_thinking","data":"sealed"},
                        {"type":"text","text":"hello"}
                    ]})
                    .to_string(),
                )
            };
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response_body.len()
            );
            stream.write_all(head.as_bytes()).await.expect("write head");
            stream
                .write_all(response_body.as_bytes())
                .await
                .expect("write body");
        });
        (addr, rx)
    }

    async fn call_proxy(base_url: &str, body: &str) -> String {
        let addr: SocketAddr = base_url
            .trim_start_matches("http://")
            .parse()
            .expect("proxy addr");
        let mut stream = TcpStream::connect(addr).await.expect("connect proxy");
        let request = format!(
            "POST /anthropic/v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nX-Api-Key: secret-key\r\nAuthorization: Bearer gw-key\r\nAccept-Encoding: gzip, deflate\r\nContent-Length: {}\r\n\r\n{b}",
            body.len(),
            b = body
        );
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

    #[tokio::test]
    async fn proxy_filters_json_request_and_response() {
        let (upstream, captured) = spawn_fake_upstream(false).await;
        let proxy = ThinkingFilterProxy::start(&format!("http://{upstream}"))
            .await
            .expect("proxy start");
        let body = json!({
            "model": "m",
            "messages": [{"role":"user","content":[
                {"type":"redacted_thinking","data":"old"},
                {"type":"text","text":"hi"}
            ]}]
        })
        .to_string();
        let response = call_proxy(&proxy.local_base_url(), &body).await;

        let captured = captured.await.expect("captured request");
        assert_eq!(captured.target, "/anthropic/v1/messages");
        assert_eq!(captured.x_api_key, "secret-key");
        assert_eq!(captured.authorization, "Bearer gw-key");
        // Compression is refused so every body can be inspected.
        assert_eq!(captured.accept_encoding, "identity");
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
        let (upstream, captured) = spawn_fake_upstream(true).await;
        let proxy = ThinkingFilterProxy::start(&format!("http://{upstream}"))
            .await
            .expect("proxy start");
        let response = call_proxy(&proxy.local_base_url(), "{}").await;

        let _captured = captured.await.expect("captured request");
        assert!(response.starts_with("HTTP/1.1 200"), "{response}");
        assert!(!response.contains("redacted_thinking"), "{response}");
        // The tool_use block originally at index 2 must be renumbered to 1.
        assert!(response.contains("\"index\":1"), "{response}");
        assert!(response.contains("tool_use"), "{response}");
        assert!(response.contains("message_stop"), "{response}");
    }

    #[tokio::test]
    async fn proxy_rejects_chunked_request_bodies() {
        let (upstream, _captured) = spawn_fake_upstream(false).await;
        let proxy = ThinkingFilterProxy::start(&format!("http://{upstream}"))
            .await
            .expect("proxy start");
        let addr: SocketAddr = proxy
            .local_base_url()
            .trim_start_matches("http://")
            .parse()
            .expect("addr");
        let mut stream = TcpStream::connect(addr).await.expect("connect proxy");
        let request = "POST /anthropic/v1/messages HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n";
        stream
            .write_all(request.as_bytes())
            .await
            .expect("write request");
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("read response");
        let response = String::from_utf8_lossy(&response);
        assert!(response.starts_with("HTTP/1.1 502"), "{response}");
        assert!(response.contains("chunked request bodies"), "{response}");
    }

    #[tokio::test]
    async fn proxy_fails_closed_on_empty_upstream() {
        let err = ThinkingFilterProxy::start("")
            .await
            .expect_err("empty upstream");
        assert!(err.to_string().contains("empty upstream"), "{err}");
    }

    #[tokio::test]
    async fn proxy_maps_paths_without_duplicating_v1() {
        let base = "https://gateway.example";
        let normalized = dsh_base_url(base);
        assert_eq!(
            upstream_url(&normalized, "/anthropic/v1/messages").expect("url"),
            "https://gateway.example/anthropic/v1/messages"
        );
        assert_eq!(
            upstream_url(&normalized, "/v1/messages").expect("url"),
            "https://gateway.example/anthropic/v1/messages"
        );
        assert_eq!(
            upstream_url(&normalized, "/messages?stream=true").expect("url"),
            "https://gateway.example/anthropic/v1/messages?stream=true"
        );
        assert!(upstream_url(&normalized, "messages").is_err());
    }
}
