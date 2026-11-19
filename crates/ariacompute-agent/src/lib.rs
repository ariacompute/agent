//! `aria_agent_ffi` — the **cloud API client** behind the Swift and Kotlin SDKs.
//!
//! This crate mirrors `sdk/js` and `sdk/python`: it is a thin HTTP client for the
//! aria-agent-cloud **beta Agents** API (`/v1/agents/...`). The agent runtime is
//! *not* embedded — there is no loop, sandbox, model client or context store in
//! the binary, so the bindings run anywhere the network does.
//!
//! Public vocabulary is the same as the JS/Python SDKs: `AriaAgent` (the agent
//! definition), `Session` (server-side conversation + memory), `run` /
//! `runStreamed` (one turn), and `memorize` / `recall` (keyed long-term memory
//! with a `cloud` / `local` / `both` backend).
//!
//! The `local` backend shells out to the `aria-memo` CLI, exactly like the other
//! SDKs, so a device without that binary gets a clear, actionable error.

mod memory;
mod transport;

use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::memory::{MemoRunner, Memory};
use crate::transport::{encode_segment, get_json, post_json, post_json_stream, ResolvedClient};

uniffi::setup_scaffolding!();

/// Result alias for the exported surface.
type FfiResult<T> = Result<T, AriaError>;

/// Typed error surfaced to Swift/Kotlin. Every transport failure is normalised so
/// callers branch on the variant instead of matching message strings.
#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum AriaError {
    /// Bad configuration (missing name, unknown backend, unparseable URL, …).
    #[error("config: {message}")]
    Config { message: String },
    /// The server rejected the API key (`401` / `403`).
    #[error("auth: {message}")]
    Auth { message: String },
    /// The request never reached the server (DNS, TLS, timeout, refused, …).
    #[error("network: {message}")]
    Network { message: String },
    /// The server answered with a non-2xx status.
    #[error("api ({status}): {message}")]
    Api { status: u16, message: String },
    /// Anything else (malformed body, CLI failure, …).
    #[error("unknown: {message}")]
    Unknown { message: String },
}

impl AriaError {
    /// Stable machine-readable kind (`config` / `auth` / `network` / `api` / `unknown`).
    pub fn kind(&self) -> String {
        match self {
            AriaError::Config { .. } => "config",
            AriaError::Auth { .. } => "auth",
            AriaError::Network { .. } => "network",
            AriaError::Api { .. } => "api",
            AriaError::Unknown { .. } => "unknown",
        }
        .to_string()
    }

    /// HTTP status when the server answered, else `None`.
    pub fn status(&self) -> Option<u16> {
        match self {
            AriaError::Api { status, .. } => Some(*status),
            _ => None,
        }
    }
}

/// Where the client points and how it authenticates.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct ClientOptions {
    /// Base URL of the cloud service (default `ARIA_AGENT_BASE_URL`, else
    /// `http://localhost:3000`).
    pub base_url: Option<String>,
    /// API key sent as `Authorization: Bearer <key>` (default `ARIA_AGENT_API_KEY`).
    pub api_key: Option<String>,
    /// Override the `OpenAI-Beta` header (default `agents=v1`).
    pub beta_header: Option<String>,
}

/// Memory backend configuration shared by the agent and its sessions.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct MemoryConfig {
    /// Default backend for `memorize` / `recall`: `cloud` | `local` | `both`.
    pub backend: Option<String>,
    /// `aria-memo` binary for the local backend (default `aria-memo`, or
    /// `ARIA_MEMO_BIN`).
    pub memo_bin: Option<String>,
    /// aria memo database path (default `memo.db`, or `ARIA_MEMO_DB`).
    pub memo_db: Option<String>,
}

/// The agent definition sent to the cloud (name, instructions, model).
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct AriaAgentConfig {
    /// Display name; the cloud resolves or creates the agent by name.
    pub name: String,
    /// Instructions appended to the agent's base prompt.
    pub instructions: Option<String>,
    /// Model override (provider-specific id).
    pub model: Option<String>,
    /// Existing cloud agent id (skips the name lookup).
    pub id: Option<String>,
    /// Client configuration.
    pub client: Option<ClientOptions>,
    /// Memory configuration inherited by sessions created from this agent.
    pub memory: Option<MemoryConfig>,
}

/// Token accounting reported for a run.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct TokenUsage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub total_tokens: u32,
    /// Model the provider reported (may differ from the requested one).
    pub model: Option<String>,
}

/// One decoded streaming event (`agent.*` frame).
///
/// `kind` is the frame type; the remaining fields are populated per kind and
/// `raw_json` always carries the untouched frame for anything not modelled here.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct StreamEvent {
    /// e.g. `agent.turn.output_text.delta`.
    pub kind: String,
    pub turn_id: Option<String>,
    /// `recall` / `harness` / `step` / `model` / `tool_exec` / `loop_guard`.
    pub phase: Option<String>,
    pub label: Option<String>,
    /// Text increment for `agent.turn.output_text.delta`.
    pub delta: Option<String>,
    /// Full text for `agent.turn.output_text.done`.
    pub text: Option<String>,
    /// `item` object (JSON) for `agent.turn.item.added` / `.done`.
    pub item_json: Option<String>,
    pub tool: Option<String>,
    /// `allow` / `deny` / `unavailable`.
    pub decision: Option<String>,
    pub reason: Option<String>,
    /// Error message for `agent.turn.failed`.
    pub error: Option<String>,
    /// Final output for `agent.turn.completed`.
    pub output: Option<String>,
    /// The untouched frame as JSON.
    pub raw_json: String,
}

/// Result of a completed run.
#[derive(Debug, Clone, Default, uniffi::Record)]
pub struct RunResult {
    /// The assistant's final reply.
    pub final_output: String,
    /// Server-side session id (reuse it to continue the conversation).
    pub session_id: String,
    /// Server-side turn id, when the server reported one.
    pub turn_id: Option<String>,
    /// Accumulated token usage, when the server reported it.
    pub usage: Option<TokenUsage>,
    /// Conversation mirror as a JSON array of `{role, content}`.
    pub history_json: String,
}

/// Streaming callback implemented on the Swift/Kotlin side.
#[uniffi::export(callback_interface)]
pub trait StreamListener: Send + Sync {
    /// Called once per SSE frame, in wire order.
    fn on_event(&self, event: StreamEvent);
}

/// The agent definition plus its client configuration.
#[derive(uniffi::Object)]
pub struct AriaAgent {
    config: AriaAgentConfig,
    client: ResolvedClient,
}

#[uniffi::export]
impl AriaAgent {
    /// Validate the config and resolve the client (config → env → defaults).
    #[uniffi::constructor]
    pub fn new(config: AriaAgentConfig) -> FfiResult<Arc<Self>> {
        if config.name.trim().is_empty() {
            return Err(AriaError::Config {
                message: "agent requires a `name`".into(),
            });
        }
        let client = transport::resolve_client(config.client.as_ref())?;
        Ok(Arc::new(Self { config, client }))
    }

    /// The agent's name.
    pub fn name(&self) -> String {
        self.config.name.clone()
    }

    /// The configured model, if any.
    pub fn model(&self) -> Option<String> {
        self.config.model.clone()
    }

    /// The cloud agent id, when the caller supplied one.
    pub fn id(&self) -> Option<String> {
        self.config.id.clone()
    }

    /// Create a server-side session bound to this agent.
    pub fn create_session(&self) -> FfiResult<Arc<Session>> {
        let mut body = serde_json::Map::new();
        match &self.config.id {
            Some(id) => {
                body.insert("agent_id".into(), json!(id));
            }
            None => {
                body.insert("agent".into(), json!(self.config.name));
            }
        }
        if let Some(instructions) = &self.config.instructions {
            body.insert("instructions".into(), json!(instructions));
        }
        let res: SessionResponse =
            post_json(&self.client, "/v1/agents/sessions", &Value::Object(body))?;
        Ok(Arc::new(Session::new(
            res.id,
            self.config.memory.clone(),
            self.client.clone(),
            None,
        )))
    }

    /// Wrap an existing server-side session id.
    pub fn session(&self, id: String) -> FfiResult<Arc<Session>> {
        if id.trim().is_empty() {
            return Err(AriaError::Config {
                message: "session id must not be empty".into(),
            });
        }
        Ok(Arc::new(Session::new(
            id,
            self.config.memory.clone(),
            self.client.clone(),
            None,
        )))
    }

    /// Run one turn to completion (`POST …/events`).
    ///
    /// `session` is created on demand when `None`.
    pub fn run(&self, input: String, session: Option<Arc<Session>>) -> FfiResult<RunResult> {
        let session = self.ensure_session(session)?;
        let turn: TurnResponse = post_json(
            &self.client,
            &format!("/v1/agents/sessions/{}/events", encode_segment(&session.id)),
            &json!({ "input": input }),
        )?;
        let output = turn.output.clone().unwrap_or_default();
        session.record(&input, &output);
        Ok(RunResult {
            final_output: output,
            session_id: session.id.clone(),
            turn_id: Some(turn.id),
            usage: None,
            history_json: session.history_json(),
        })
    }

    /// Run one turn, streaming every `agent.*` frame to `listener`.
    ///
    /// Returns once the terminal frame arrives. `max_turns` is a client-side
    /// safety cap on `agent.turn.created` frames (the cloud controls multi-step
    /// execution); it must be a positive integer when provided.
    pub fn run_streamed(
        &self,
        input: String,
        session: Option<Arc<Session>>,
        listener: Box<dyn StreamListener>,
        max_turns: Option<u32>,
    ) -> FfiResult<RunResult> {
        if max_turns == Some(0) {
            return Err(AriaError::Config {
                message: "maxTurns must be a positive integer".into(),
            });
        }
        let session = self.ensure_session(session)?;
        let mut state = StreamState::default();

        let result = post_json_stream(
            &self.client,
            &format!(
                "/v1/agents/sessions/{}/events/stream",
                encode_segment(&session.id)
            ),
            &json!({ "input": input }),
            |frame| {
                let event = decode_event(&frame);
                if let Some(turn_id) = &event.turn_id {
                    state.turn_id = Some(turn_id.clone());
                }
                match event.kind.as_str() {
                    "agent.turn.created" => {
                        state.turns += 1;
                        if max_turns.is_some_and(|max| state.turns > max) {
                            state.failure =
                                Some(format!("exceeded maxTurns ({})", max_turns.unwrap_or(0)));
                            return Err(AriaError::Config {
                                message: state.failure.clone().unwrap_or_default(),
                            });
                        }
                    }
                    "agent.turn.output_text.delta" => {
                        state
                            .text
                            .push_str(event.delta.as_deref().unwrap_or_default());
                    }
                    "agent.turn.output_text.done" | "agent.turn.completed" => {
                        if let Some(text) = event.text.clone().or_else(|| event.output.clone()) {
                            if !text.is_empty() {
                                state.text = text;
                            }
                        }
                    }
                    "agent.turn.usage" => {
                        state.usage = frame_usage(&frame);
                    }
                    "agent.turn.failed" => {
                        state.failure = Some(
                            event
                                .error
                                .clone()
                                .unwrap_or_else(|| "run failed".to_string()),
                        );
                    }
                    _ => {}
                }
                listener.on_event(event);
                Ok(())
            },
        );

        match result {
            Ok(frames) => {
                if let Some(message) = state.failure.take() {
                    return Err(failure_error(message));
                }
                if frames == 0 {
                    return Err(AriaError::Unknown {
                        message: "the stream ended without any event".into(),
                    });
                }
                session.record(&input, &state.text);
                Ok(RunResult {
                    final_output: state.text.clone(),
                    session_id: session.id.clone(),
                    turn_id: state.turn_id.clone(),
                    usage: state.usage.clone(),
                    history_json: session.history_json(),
                })
            }
            // A terminal `agent.turn.failed` frame still surfaces as a run error.
            Err(err) => match state.failure.take() {
                Some(message) => Err(AriaError::Api {
                    status: 200,
                    message,
                }),
                None => Err(err),
            },
        }
    }
}

impl AriaAgent {
    fn ensure_session(&self, session: Option<Arc<Session>>) -> FfiResult<Arc<Session>> {
        match session {
            Some(session) => Ok(session),
            None => self.create_session(),
        }
    }
}

#[derive(serde::Deserialize)]
struct SessionResponse {
    id: String,
}

#[derive(serde::Deserialize)]
struct TurnResponse {
    id: String,
    #[serde(default)]
    output: Option<String>,
}

/// Mutable state accumulated while a stream is consumed.
#[derive(Default)]
struct StreamState {
    text: String,
    turn_id: Option<String>,
    usage: Option<TokenUsage>,
    turns: u32,
    failure: Option<String>,
}

/// A server-side session: conversation id plus its keyed memory.
#[derive(uniffi::Object)]
pub struct Session {
    id: String,
    memory: MemoryConfig,
    client: ResolvedClient,
    history: Mutex<Vec<Value>>,
    runner: Option<Arc<dyn MemoRunner>>,
}

#[uniffi::export]
impl Session {
    /// The server-side session id.
    pub fn id(&self) -> String {
        self.id.clone()
    }

    /// The memory backend used when a call does not override it.
    pub fn memory_backend(&self) -> String {
        self.memory
            .backend
            .clone()
            .unwrap_or_else(|| "cloud".into())
    }

    /// Write one keyed long-term memory (`backend` overrides the default).
    pub fn memorize(&self, key: String, value: String, backend: Option<String>) -> FfiResult<()> {
        self.store(backend)?.put(&key, &value)
    }

    /// Read one keyed long-term memory (`None` when the key is missing).
    pub fn recall(&self, key: String, backend: Option<String>) -> FfiResult<Option<String>> {
        self.store(backend)?.get(&key)
    }

    /// `GET …/items` as a JSON array.
    pub fn items_json(&self) -> FfiResult<String> {
        self.get_json("/items")
    }

    /// `GET …/turns` as a JSON array.
    pub fn turns_json(&self) -> FfiResult<String> {
        self.get_json("/turns")
    }

    /// `GET …/title` as a JSON object (404 before the first turn).
    pub fn title_json(&self) -> FfiResult<String> {
        self.get_json("/title")
    }

    /// The client-side conversation mirror (`[{role, content}]`).
    pub fn history_json(&self) -> String {
        let history = self.history.lock().map(|h| h.clone()).unwrap_or_default();
        Value::Array(history).to_string()
    }
}

impl Session {
    pub(crate) fn new(
        id: String,
        memory: Option<MemoryConfig>,
        client: ResolvedClient,
        runner: Option<Arc<dyn MemoRunner>>,
    ) -> Self {
        Self {
            id,
            memory: memory.unwrap_or_default(),
            client,
            history: Mutex::new(Vec::new()),
            runner,
        }
    }

    fn store(&self, backend: Option<String>) -> FfiResult<Memory> {
        let backend = backend
            .filter(|b| !b.trim().is_empty())
            .or_else(|| self.memory.backend.clone())
            .unwrap_or_else(|| "cloud".to_string());
        Memory::new(
            backend,
            self.id.clone(),
            self.client.clone(),
            Some(&self.memory),
            self.runner.clone(),
        )
    }

    fn get_json(&self, suffix: &str) -> FfiResult<String> {
        let value: Value = get_json(
            &self.client,
            &format!("/v1/agents/sessions/{}{suffix}", encode_segment(&self.id)),
        )?;
        Ok(value.to_string())
    }

    fn record(&self, input: &str, output: &str) {
        if let Ok(mut history) = self.history.lock() {
            history.push(json!({ "role": "user", "content": input }));
            history.push(json!({ "role": "assistant", "content": output }));
        }
    }
}

/// Map an accumulated stream failure onto the right error variant.
///
/// The `maxTurns` guard is local misuse (`config`); anything the server reported
/// as a failed turn is an `api` error even though the stream itself was `200`.
fn failure_error(message: String) -> AriaError {
    if message.starts_with("exceeded maxTurns") {
        return AriaError::Config { message };
    }
    AriaError::Api {
        status: 200,
        message,
    }
}

/// Normalise a `usage` object from a frame (`snake_case` on the wire).
fn frame_usage(frame: &Value) -> Option<TokenUsage> {
    let raw = frame.get("usage")?;
    let input = raw.get("input_tokens").and_then(Value::as_u64).unwrap_or(0);
    let output = raw
        .get("output_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let total = raw
        .get("total_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(input + output);
    Some(TokenUsage {
        input_tokens: u32::try_from(input).unwrap_or(u32::MAX),
        output_tokens: u32::try_from(output).unwrap_or(u32::MAX),
        total_tokens: u32::try_from(total).unwrap_or(u32::MAX),
        model: frame
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

fn text_of(value: Option<&Value>) -> Option<String> {
    value.and_then(Value::as_str).map(str::to_string)
}

/// Narrow a raw frame to the typed [`StreamEvent`] (unknown types pass through).
pub(crate) fn decode_event(frame: &Value) -> StreamEvent {
    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let item_json = frame.get("item").map(Value::to_string);
    StreamEvent {
        turn_id: text_of(frame.get("turn_id")),
        phase: text_of(frame.get("phase")),
        label: text_of(frame.get("label")),
        delta: text_of(frame.get("delta")),
        text: text_of(frame.get("text")),
        item_json,
        tool: text_of(frame.get("tool")),
        decision: text_of(frame.get("decision")),
        reason: text_of(frame.get("reason")),
        error: frame
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(Value::as_str)
            .map(str::to_string),
        output: frame
            .get("turn")
            .and_then(|t| t.get("output"))
            .and_then(Value::as_str)
            .map(str::to_string),
        raw_json: frame.to_string(),
        kind,
    }
}

#[cfg(test)]
mod tests;
