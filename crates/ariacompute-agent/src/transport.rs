//! HTTP transport for the aria-agent-cloud **beta Agents** API.
//!
//! This crate is a **cloud API client**, exactly like `sdk/js` and `sdk/python`:
//! it never embeds the runtime. Every call is an HTTP request to
//! `/v1/agents/...` (JSON in, JSON or SSE out), so the Swift/Kotlin bindings work
//! on a phone with nothing but network access.

use std::time::Duration;

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::AriaError;

/// Default beta header required by the OpenAI-compatible surface.
pub(crate) const DEFAULT_BETA_HEADER: &str = "agents=v1";
/// Default base URL when neither config nor env supplies one.
pub(crate) const DEFAULT_BASE_URL: &str = "http://localhost:3000";
/// Connection/read timeout for a single request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// A client resolved from defaults, config and environment.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedClient {
    pub(crate) base_url: String,
    pub(crate) api_key: Option<String>,
    pub(crate) beta_header: String,
    pub(crate) http: reqwest::blocking::Client,
}

/// Resolve client options the same way the JS/Python SDKs do.
///
/// Precedence: explicit config → environment (`ARIA_AGENT_BASE_URL`,
/// `ARIA_AGENT_API_KEY`) → built-in defaults. A trailing `/` is stripped so
/// paths never double up.
pub(crate) fn resolve_client(
    options: Option<&crate::ClientOptions>,
) -> Result<ResolvedClient, AriaError> {
    // `Client::new()` cannot fail; the timeout is applied per request instead.
    let http = reqwest::blocking::Client::new();
    let base_url = options
        .and_then(|o| o.base_url.clone())
        .or_else(|| env_non_empty("ARIA_AGENT_BASE_URL"))
        .unwrap_or_else(|| DEFAULT_BASE_URL.to_string());
    let base_url = base_url.trim().trim_end_matches('/').to_string();
    if base_url.is_empty() {
        return Err(AriaError::Config {
            detail: "base_url must not be empty".into(),
        });
    }
    Ok(ResolvedClient {
        base_url,
        api_key: options
            .and_then(|o| o.api_key.clone())
            .or_else(|| env_non_empty("ARIA_AGENT_API_KEY")),
        beta_header: options
            .and_then(|o| o.beta_header.clone())
            .filter(|h| !h.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_BETA_HEADER.to_string()),
        http,
    })
}

fn env_non_empty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Percent-encode one path segment (`sessions/{id}`, `memory/{key}`).
pub(crate) fn encode_segment(value: &str) -> String {
    const KEEP: &[u8] = b"-_.~";
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if byte.is_ascii_alphanumeric() || KEEP.contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

impl ResolvedClient {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::blocking::RequestBuilder {
        let mut builder = self
            .http
            .request(method, format!("{}{}", self.base_url, path))
            .timeout(REQUEST_TIMEOUT)
            .header("OpenAI-Beta", &self.beta_header);
        if let Some(key) = &self.api_key {
            builder = builder.bearer_auth(key);
        }
        builder
    }
}

/// `POST` a JSON body and decode a JSON response.
pub(crate) fn post_json<T: DeserializeOwned>(
    client: &ResolvedClient,
    path: &str,
    body: &Value,
) -> Result<T, AriaError> {
    let res = client
        .request(reqwest::Method::POST, path)
        .json(body)
        .send()
        .map_err(network_error)?;
    decode_json(res)
}

/// `GET` a JSON response.
pub(crate) fn get_json<T: DeserializeOwned>(
    client: &ResolvedClient,
    path: &str,
) -> Result<T, AriaError> {
    let res = client
        .request(reqwest::Method::GET, path)
        .send()
        .map_err(network_error)?;
    decode_json(res)
}

fn decode_json<T: DeserializeOwned>(res: reqwest::blocking::Response) -> Result<T, AriaError> {
    let status = res.status();
    if !status.is_success() {
        return Err(api_error(status.as_u16(), &res.text().unwrap_or_default()));
    }
    res.json::<T>().map_err(|e| AriaError::Unknown {
        detail: format!("cannot decode the response: {e}"),
    })
}

fn network_error(err: reqwest::Error) -> AriaError {
    AriaError::Network {
        detail: format!("aria agent request failed (network): {err}"),
    }
}

/// Turn an error body into a typed error, preferring the server's `message`.
pub(crate) fn api_error(status: u16, body: &str) -> AriaError {
    let message = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .or_else(|| v.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| {
            if body.trim().is_empty() {
                format!("request failed with status {status}")
            } else {
                body.trim().to_string()
            }
        });
    if status == 401 || status == 403 {
        AriaError::Auth { detail: message }
    } else {
        AriaError::Api {
            status,
            detail: message,
        }
    }
}

/// `POST` a JSON body and consume an SSE response, calling `on_frame` per event.
///
/// Frames are the beta Agents `agent.*` objects (`data: {...}`); the `[DONE]`
/// sentinel terminates the stream early when present. Returns the number of
/// frames delivered.
pub(crate) fn post_json_stream(
    client: &ResolvedClient,
    path: &str,
    body: &Value,
    on_frame: impl FnMut(Value) -> Result<(), AriaError>,
) -> Result<usize, AriaError> {
    let res = client
        .request(reqwest::Method::POST, path)
        .json(body)
        .send()
        .map_err(network_error)?;
    let status = res.status();
    if !status.is_success() {
        return Err(api_error(status.as_u16(), &res.text().unwrap_or_default()));
    }
    read_frames(std::io::BufReader::new(res), on_frame)
}

/// Consume framed SSE lines from any reader (a response body, or a test stub).
///
/// Frames are the beta Agents `agent.*` objects (`data: {...}`); the `[DONE]`
/// sentinel terminates the stream early when present. Returns the number of
/// frames delivered.
pub(crate) fn read_frames(
    mut reader: impl std::io::BufRead,
    mut on_frame: impl FnMut(Value) -> Result<(), AriaError>,
) -> Result<usize, AriaError> {
    let mut frames = 0usize;
    let mut data: Vec<String> = Vec::new();
    let mut line = String::new();
    loop {
        line.clear();
        let read = reader
            .read_line(&mut line)
            .map_err(|e| AriaError::Network {
                detail: format!("aria agent stream read failed: {e}"),
            })?;
        if read == 0 {
            break;
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            if !dispatch(&mut data, &mut on_frame, &mut frames)? {
                return Ok(frames);
            }
            continue;
        }
        if let Some(payload) = line.strip_prefix("data:") {
            data.push(payload.trim_start().to_string());
        }
        // `event:` / `id:` / `:` comment lines are ignored: the payload carries type.
    }
    // A stream that ends without a trailing blank line still has one last frame.
    dispatch(&mut data, &mut on_frame, &mut frames)?;
    Ok(frames)
}

/// Deliver one buffered SSE event; returns `false` when `[DONE]` arrived.
fn dispatch(
    data: &mut Vec<String>,
    on_frame: &mut impl FnMut(Value) -> Result<(), AriaError>,
    frames: &mut usize,
) -> Result<bool, AriaError> {
    if data.is_empty() {
        return Ok(true);
    }
    let payload = data.join("\n");
    data.clear();
    if payload.trim() == "[DONE]" {
        return Ok(false);
    }
    let value: Value = serde_json::from_str(&payload).map_err(|e| AriaError::Unknown {
        detail: format!("cannot decode a stream frame: {e}"),
    })?;
    on_frame(value)?;
    *frames += 1;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_segments_are_encoded() {
        assert_eq!(encode_segment("plain-1_2.3~"), "plain-1_2.3~");
        assert_eq!(encode_segment("a b/c?d"), "a%20b%2Fc%3Fd");
        assert_eq!(encode_segment("会话"), "%E4%BC%9A%E8%AF%9D");
    }

    #[test]
    fn error_bodies_are_normalised() {
        let auth = api_error(401, r#"{"error":{"message":"nope"}}"#);
        assert!(matches!(auth, AriaError::Auth { .. }), "{auth}");
        let forbidden = api_error(403, "");
        assert!(matches!(forbidden, AriaError::Auth { .. }), "{forbidden}");
        let api = api_error(500, r#"{"message":"boom"}"#);
        assert_eq!(api.to_string(), "api (500): boom");
        assert_eq!(api.status(), Some(500));
        let plain = api_error(502, "bad gateway");
        assert!(
            matches!(plain, AriaError::Api { status: 502, .. }),
            "{plain}"
        );
        let empty = api_error(400, "   ");
        assert!(matches!(empty, AriaError::Api { detail, .. } if detail.contains("400")));
    }

    #[test]
    fn a_client_strips_trailing_slashes_and_rejects_an_empty_base() {
        let resolved = resolve_client(Some(&crate::ClientOptions {
            base_url: Some("http://example.test/".into()),
            api_key: Some("k".into()),
            beta_header: Some("  ".into()),
        }))
        .expect("resolves");
        assert_eq!(resolved.base_url, "http://example.test");
        assert_eq!(resolved.api_key.as_deref(), Some("k"));
        assert_eq!(resolved.beta_header, DEFAULT_BETA_HEADER);

        for blank in ["  ", ""] {
            let err = resolve_client(Some(&crate::ClientOptions {
                base_url: Some(blank.into()),
                api_key: None,
                beta_header: None,
            }))
            .expect_err("blank base must fail");
            assert!(matches!(err, AriaError::Config { .. }), "{err}");
        }
    }

    #[test]
    fn a_custom_beta_header_is_kept() {
        let resolved = resolve_client(Some(&crate::ClientOptions {
            base_url: Some("http://example.test".into()),
            api_key: None,
            beta_header: Some("agents=v2".into()),
        }))
        .expect("resolves");
        assert_eq!(resolved.beta_header, "agents=v2");
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;

    /// A reader whose first read fails, so the SSE error path is exercised.
    struct BrokenReader;

    impl std::io::Read for BrokenReader {
        fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("EIO"))
        }
    }

    #[test]
    fn frames_are_read_leniently() {
        // Multi-line data, a comment, and a final frame without a blank line.
        let body = "event: agent.turn.created\ndata: {\"type\":\ndata: \"agent.turn.created\"}\n\n\
                    : comment\n\
                    data: {\"type\":\"agent.turn.completed\"}";
        let mut seen = Vec::new();
        let frames = read_frames(body.as_bytes(), |frame| {
            seen.push(frame);
            Ok(())
        })
        .expect("frames");
        assert_eq!(frames, 2, "the last frame has no trailing blank line");
        assert_eq!(seen[0]["type"], "agent.turn.created");
        assert_eq!(seen[1]["type"], "agent.turn.completed");

        let mut seen = Vec::new();
        let frames = read_frames(
            "data: {\"type\":\"x\"}\n\ndata: [DONE]\n\ndata: {\"type\":\"y\"}\n\n".as_bytes(),
            |frame| {
                seen.push(frame);
                Ok(())
            },
        )
        .expect("frames");
        assert_eq!(frames, 1);
        assert_eq!(seen.len(), 1);
    }

    #[test]
    fn a_read_error_and_a_bad_frame_surface() {
        let err =
            read_frames(std::io::BufReader::new(BrokenReader), |_| Ok(())).expect_err("must fail");
        assert!(err.to_string().contains("stream read failed"), "{err}");
        let err = read_frames("data: nope\n\n".as_bytes(), |_| Ok(())).expect_err("must fail");
        assert!(err.to_string().contains("stream frame"), "{err}");
    }

    #[test]
    fn a_callback_error_stops_the_stream() {
        let err = read_frames(
            "data: {\"type\":\"a\"}\n\ndata: {\"type\":\"b\"}\n\n".as_bytes(),
            |_| {
                Err(AriaError::Unknown {
                    detail: "stop".into(),
                })
            },
        )
        .expect_err("must fail");
        assert!(err.to_string().contains("stop"), "{err}");
    }

    /// Serialises the one test that mutates the client env.
    static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn environment_defaults_fill_the_client() {
        let _guard = ENV_GUARD.lock().expect("env guard");
        std::env::set_var("ARIA_AGENT_BASE_URL", "https://env.test/");
        std::env::set_var("ARIA_AGENT_API_KEY", "env-key");
        let resolved = resolve_client(None).expect("resolves");
        assert_eq!(resolved.base_url, "https://env.test");
        assert_eq!(resolved.api_key.as_deref(), Some("env-key"));

        std::env::remove_var("ARIA_AGENT_BASE_URL");
        std::env::remove_var("ARIA_AGENT_API_KEY");
        let resolved = resolve_client(None).expect("resolves");
        assert_eq!(resolved.base_url, DEFAULT_BASE_URL);
        assert!(resolved.api_key.is_none());
    }
}
