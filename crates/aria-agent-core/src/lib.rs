//! `agent-core` — the unified agent runtime.
//!
//! It wraps the codex harness-style agent loop and guarantees the **memo
//! contract**: every [`Agent::run`] first [`recall`](agent_memo::MemoStore::recall)s
//! context from memo (now vector/semantic recall — see `agent_memo::embed`), then
//! after producing a reply [`memorize`](agent_memo::MemoStore::memorize)s
//! both the user turn and the assistant reply. Tool execution is isolated in a
//! [`Sandbox`](agent_sandbox::Sandbox).
//!
//! The LLM is accessed through [`ModelClient`]; a deterministic [`StubModel`]
//! is the default so the runtime is runnable without an API key. Enable the
//! `openai` feature to use the real OpenAI Responses/Chat API.

use agent_memo::{ContextFragment, FragmentKind, MemoStore, RecallQuery, SledMemoStore};
use agent_sandbox::{default_sandbox, Sandbox, SandboxProvider};
use async_trait::async_trait;
use futures::stream::{BoxStream, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::{Arc, RwLock};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CoreError {
    #[error("memo error: {0}")]
    Memo(#[from] agent_memo::MemoError),
    #[error("sandbox error: {0}")]
    Sandbox(#[from] agent_sandbox::SandboxError),
    #[error("model error: {0}")]
    Model(String),
    #[error("config error: {0}")]
    Config(String),
}

/// A single model request.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelRequest {
    pub system: String,
    pub context: String,
    pub input: String,
}

/// A single model response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelResponse {
    pub text: String,
}

/// A tool the agent may invoke during an agentic turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Tool {
    pub name: String,
    pub description: String,
    /// JSON-schema object describing the tool's parameters.
    pub parameters: Value,
}

/// A request from the model to invoke a tool.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Parsed tool arguments (typically a JSON object).
    pub arguments: Value,
}

/// The outcome of executing a tool, fed back to the model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    pub is_error: bool,
}

/// A model reply that may request one or more tool calls (agentic loop).
#[derive(Debug, Clone, Default)]
pub struct ModelTurn {
    pub text: String,
    pub tool_calls: Vec<ToolCall>,
}

/// Events emitted by [`Agent::run_event_stream`] so callers can render a live
/// agentic turn: phase boundaries, tool calls (with their results), streamed
/// tokens, and the terminal `Done`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A phase boundary: `recall`, `model`, `tool_exec`, `loop_guard`.
    Step { phase: String, label: Option<String> },
    /// A tool invocation and its (already executed) result.
    ToolCall {
        id: String,
        name: String,
        arguments: Value,
        result: ToolResult,
    },
    /// A model text delta.
    Token { text: String },
    /// Terminal event carrying the full final reply.
    Done { text: String },
}

/// LLM access seam. Implement this to plug in any backend.
#[async_trait]
pub trait ModelClient: Send + Sync {
    /// Produce a complete reply in one shot.
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError>;

    /// Stream the reply as a sequence of tokens. The default implementation
    /// yields the full [`ModelResponse::text`] as a single chunk, so backends
    /// that only support non-streaming calls work unchanged.
    async fn stream(
        &self,
        req: &ModelRequest,
    ) -> Result<BoxStream<'static, Result<String, CoreError>>, CoreError> {
        let resp = self.complete(req).await?;
        Ok(Box::pin(futures::stream::once(
            async move { Ok(resp.text) },
        )))
    }

    /// Produce a reply that may request tool calls (agentic loop). The default
    /// implementation ignores `tools` and delegates to
    /// [`ModelClient::complete`], so backends without function-calling support
    /// degrade to a single-shot reply. Override this to drive the agentic loop.
    async fn complete_with_tools(
        &self,
        req: &ModelRequest,
        _tools: &[Tool],
    ) -> Result<ModelTurn, CoreError> {
        let resp = self.complete(req).await?;
        Ok(ModelTurn {
            text: resp.text,
            tool_calls: Vec::new(),
        })
    }
}

/// Deterministic stand-in used when no API key / `openai` feature is present.
pub struct StubModel {
    agent_name: String,
}

impl StubModel {
    pub fn new(agent_name: &str) -> Self {
        Self {
            agent_name: agent_name.to_string(),
        }
    }
}

#[async_trait]
impl ModelClient for StubModel {
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
        let text = format!(
            "[{}] (stub) context={} | input={}",
            self.agent_name,
            if req.context.is_empty() {
                "<none>"
            } else {
                "<injected>"
            },
            req.input
        );
        Ok(ModelResponse { text })
    }
}

#[cfg(feature = "openai")]
pub use openai_impl::OpenAiModel;

#[cfg(feature = "openai")]
mod openai_impl {
    use super::*;
    use async_openai::types::{
        ChatCompletionRequestMessage, ChatCompletionRequestSystemMessage,
        ChatCompletionRequestUserMessage, ChatCompletionRequestUserMessageContent,
    };
    use async_openai::{config::OpenAIConfig, Client};

    /// Calls the OpenAI Chat Completions API as the agent's model backend.
    pub struct OpenAiModel {
        client: Client<OpenAIConfig>,
        model: String,
    }

    impl OpenAiModel {
        pub fn new(model: &str) -> Self {
            let api_key = std::env::var("OPENAI_API_KEY").unwrap_or_default();
            let config = OpenAIConfig::new().with_api_key(api_key);
            Self {
                client: Client::with_config(config),
                model: model.to_string(),
            }
        }
    }

    #[async_trait]
    impl ModelClient for OpenAiModel {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            use async_openai::types::CreateChatCompletionRequestArgs;
            let messages = vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: req.system.clone().into(),
                    ..Default::default()
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(format!(
                        "{}\n\nUSER: {}",
                        req.context, req.input
                    )),
                    ..Default::default()
                }),
            ];
            let request = CreateChatCompletionRequestArgs::default()
                .model(self.model.clone())
                .messages(messages)
                .build()
                .map_err(|e| CoreError::Model(e.to_string()))?;
            let resp = self
                .client
                .chat()
                .create(request)
                .await
                .map_err(|e| CoreError::Model(e.to_string()))?;
            let text = resp
                .choices
                .first()
                .and_then(|c| c.message.content.clone())
                .unwrap_or_default();
            Ok(ModelResponse { text })
        }

        async fn stream(
            &self,
            req: &ModelRequest,
        ) -> Result<BoxStream<'static, Result<String, CoreError>>, CoreError> {
            use async_openai::types::CreateChatCompletionRequestArgs;
            use futures::StreamExt as _;
            let messages = vec![
                ChatCompletionRequestMessage::System(ChatCompletionRequestSystemMessage {
                    content: req.system.clone().into(),
                    ..Default::default()
                }),
                ChatCompletionRequestMessage::User(ChatCompletionRequestUserMessage {
                    content: ChatCompletionRequestUserMessageContent::Text(format!(
                        "{}\n\nUSER: {}",
                        req.context, req.input
                    )),
                    ..Default::default()
                }),
            ];
            let request = CreateChatCompletionRequestArgs::default()
                .model(self.model.clone())
                .messages(messages)
                .stream(true)
                .build()
                .map_err(|e| CoreError::Model(e.to_string()))?;
            let client = self.client.clone();
            let s = async_stream::stream! {
                let mut stream = match client.chat().create_stream(request).await {
                    Ok(s) => s,
                    Err(e) => {
                        yield Err(CoreError::Model(e.to_string()));
                        return;
                    }
                };
                while let Some(chunk) = stream.next().await {
                    match chunk {
                        Ok(resp) => {
                            if let Some(tok) = resp
                                .choices
                                .into_iter()
                                .next()
                                .and_then(|c| c.delta.content)
                            {
                                yield Ok(tok);
                            }
                        }
                        Err(e) => yield Err(CoreError::Model(e.to_string())),
                    }
                }
            };
            Ok(Box::pin(s))
        }
    }
}

/// Configuration for constructing an [`Agent`]. Serializable for SDK/Ffi.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub session: String,
    pub agent_name: String,
    pub sandbox_provider: String,
    pub model: String,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            session: "default".to_string(),
            agent_name: "agent".to_string(),
            sandbox_provider: "docker".to_string(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

/// A single named skill — a reusable capability the agent may draw on.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Skill {
    pub name: String,
    pub body: String,
}

/// A single named rule — a constraint the agent must obey.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Rule {
    pub name: String,
    pub body: String,
}

/// The evolvable harness of an agent: system prompt plus its skills and rules.
/// This is the unit Reef-style self-improvement evolves, versions in Git, and
/// hot-serves back to the running agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Harness {
    pub system_prompt: String,
    pub skills: Vec<Skill>,
    pub rules: Vec<Rule>,
}

impl Harness {
    /// Baseline harness equivalent to the legacy hardcoded system prompt.
    pub fn baseline(agent_name: &str) -> Self {
        Self {
            system_prompt: format!("You are {}.", agent_name),
            skills: Vec::new(),
            rules: Vec::new(),
        }
    }

    /// True when this harness is byte-for-byte the generated baseline.
    pub fn is_baseline(&self, agent_name: &str) -> bool {
        *self == Harness::baseline(agent_name)
    }

    /// Compose the system prompt sent to the model from the harness parts.
    pub fn system_text(&self) -> String {
        let mut s = self.system_prompt.clone();
        if !self.skills.is_empty() {
            s.push_str("\n\n## Skills");
            for sk in &self.skills {
                s.push_str(&format!("\n- {}: {}", sk.name, sk.body));
            }
        }
        if !self.rules.is_empty() {
            s.push_str("\n\n## Rules");
            for r in &self.rules {
                s.push_str(&format!("\n- {}: {}", r.name, r.body));
            }
        }
        s
    }
}

/// Hot-swappable handle to the currently-served harness.
///
/// Read-heavy, write-rare: [`get`](ActiveHarness::get) only clones an `Arc`
/// (O(1), no async, no serialization), while [`set`](ActiveHarness::set)
/// atomically replaces the served harness so subsequent turns pick it up
/// without restarting the process.
pub struct ActiveHarness {
    current: Arc<RwLock<Arc<Harness>>>,
}

impl ActiveHarness {
    pub fn new(initial: Harness) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(initial))),
        }
    }

    /// Baseline harness equivalent to the legacy system prompt.
    pub fn baseline(agent_name: &str) -> Self {
        Self::new(Harness::baseline(agent_name))
    }

    /// Clone the inner `Arc` (O(1)); no serialization, no async.
    pub fn get(&self) -> Arc<Harness> {
        self.current
            .read()
            .expect("active harness lock poisoned")
            .clone()
    }

    /// Atomically replace the served harness with a new version.
    pub fn set(&self, h: Harness) {
        let mut g = self.current.write().expect("active harness lock poisoned");
        *g = Arc::new(h);
    }
}

/// The unified agent. Holds the model client, memo store, sandbox, and a
/// shared handle to the currently-served [`ActiveHarness`] (which may be
/// hot-swapped by a self-improvement loop).
pub struct Agent {
    config: AgentConfig,
    model: Arc<dyn ModelClient>,
    memo: Arc<dyn MemoStore>,
    sandbox: Arc<dyn Sandbox>,
    harness: Arc<ActiveHarness>,
}

impl Agent {
    /// Build with an explicit model client (e.g. [`StubModel`] or [`OpenAiModel`])
    /// and a shared, hot-swappable harness handle.
    pub fn with_harness(
        config: AgentConfig,
        model: Box<dyn ModelClient>,
        memo: Arc<dyn MemoStore>,
        harness: Arc<ActiveHarness>,
    ) -> Result<Self, CoreError> {
        let provider = SandboxProvider::parse(&config.sandbox_provider).ok_or_else(|| {
            CoreError::Config(format!("unknown sandbox: {}", config.sandbox_provider))
        })?;
        let sandbox = Arc::from(agent_sandbox::from_provider(provider));
        Ok(Self {
            config,
            model: Arc::from(model),
            memo,
            sandbox,
            harness,
        })
    }

    /// Build with an explicit sandbox backend (e.g. a local test sandbox or a
    /// codex-backed [`Sandbox`](agent_sandbox::Sandbox)). Useful when the
    /// provider string alone does not describe the execution environment.
    pub fn with_sandbox(
        config: AgentConfig,
        model: Box<dyn ModelClient>,
        memo: Arc<dyn MemoStore>,
        harness: Arc<ActiveHarness>,
        sandbox: Arc<dyn Sandbox>,
    ) -> Result<Self, CoreError> {
        Ok(Self {
            config,
            model: Arc::from(model),
            memo,
            sandbox,
            harness,
        })
    }

    /// Build with an explicit model client (e.g. [`StubModel`] or [`OpenAiModel`]).
    /// A baseline harness is generated from the config's agent name.
    pub fn with_model(
        config: AgentConfig,
        model: Box<dyn ModelClient>,
        memo: Arc<dyn MemoStore>,
    ) -> Result<Self, CoreError> {
        let harness = Arc::new(ActiveHarness::baseline(&config.agent_name));
        Self::with_harness(config, model, memo, harness)
    }

    /// Build using the default model (stub unless `openai` feature is on).
    pub fn new(config: AgentConfig, memo: Arc<dyn MemoStore>) -> Result<Self, CoreError> {
        let model: Box<dyn ModelClient> = {
            #[cfg(feature = "openai")]
            {
                Box::new(OpenAiModel::new(&config.model))
            }
            #[cfg(not(feature = "openai"))]
            {
                let _ = &config.model;
                Box::new(StubModel::new(&config.agent_name))
            }
        };
        Self::with_model(config, model, memo)
    }

    pub fn session(&self) -> &str {
        &self.config.session
    }

    /// Shared harness handle — hot-swappable by a self-improvement loop.
    pub fn harness(&self) -> Arc<ActiveHarness> {
        self.harness.clone()
    }

    /// Shared memo store (used by the SDK to expose `Session`).
    pub fn memo(&self) -> Arc<dyn MemoStore> {
        self.memo.clone()
    }

    /// Run one turn. Injects memo context, calls the model, persists both turns.
    pub async fn run(&self, input: &str) -> Result<String, CoreError> {
        // 1) recall context from memo (the ONLY context source)
        let fragments = self
            .memo
            .recall(&RecallQuery::new(&self.config.session, input))
            .await?;
        let context = fragments
            .iter()
            .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
            .collect::<Vec<_>>()
            .join("\n");

        // 2) persist the user turn
        self.memo
            .memorize(ContextFragment::new(
                &self.config.session,
                FragmentKind::Message,
                input,
            ))
            .await?;

        // 3) call the model
        let system = self.harness.get().system_text();
        let req = ModelRequest {
            system,
            context,
            input: input.to_string(),
        };
        let resp = self.model.complete(&req).await?;

        // 4) persist the assistant reply
        self.memo
            .memorize(ContextFragment::new(
                &self.config.session,
                FragmentKind::Message,
                resp.text.clone(),
            ))
            .await?;

        Ok(resp.text)
    }

    /// Run a shell command inside the sandbox and remember the result.
    pub async fn exec_tool(&self, command: &[String]) -> Result<String, CoreError> {
        let spec = agent_sandbox::ExecSpec::command(command.to_vec());
        let handle = self.sandbox.spawn(&spec).await?;
        let out = self.sandbox.exec(&handle, command).await?;
        self.sandbox.destroy(handle).await?;
        let captured = format!(
            "exit={} stdout={} stderr={}",
            out.exit_code, out.stdout, out.stderr
        );
        self.memo
            .memorize(ContextFragment::new(
                &self.config.session,
                FragmentKind::ToolResult,
                captured.clone(),
            ))
            .await?;
        Ok(captured)
    }

    /// Stream one turn token-by-token. Mirrors [`Agent::run`] for the memo
    /// contract (recall before / persist both turns after) but emits model
    /// tokens as they arrive. The returned stream is `'static` and owns its
    /// memo handle and model client, so the [`Agent`] may be dropped.
    pub async fn run_stream(
        &self,
        input: &str,
    ) -> Result<BoxStream<'static, Result<String, CoreError>>, CoreError> {
        // 1) recall context from memo (the ONLY context source)
        let fragments = self
            .memo
            .recall(&RecallQuery::new(&self.config.session, input))
            .await?;
        let context = fragments
            .iter()
            .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
            .collect::<Vec<_>>()
            .join("\n");

        // 2) persist the user turn
        self.memo
            .memorize(ContextFragment::new(
                &self.config.session,
                FragmentKind::Message,
                input,
            ))
            .await?;

        // 3) call the model (streaming). The returned stream is owned / 'static.
        let system = self.harness.get().system_text();
        let req = ModelRequest {
            system,
            context,
            input: input.to_string(),
        };
        let upstream = self.model.stream(&req).await?;

        // 4) wrap so we can collect + persist the assistant reply at the end.
        let memo = self.memo.clone();
        let session = self.config.session.clone();
        let wrapped = async_stream::stream! {
            let mut collected = String::new();
            let mut upstream = upstream;
            while let Some(item) = upstream.next().await {
                match item {
                    Ok(tok) => {
                        collected.push_str(&tok);
                        yield Ok(tok);
                    }
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                }
            }
            // persist the assistant reply as a single memo fragment
            let _ = memo
                .memorize(ContextFragment::new(&session, FragmentKind::Message, collected))
                .await;
        };
        Ok(Box::pin(wrapped))
    }

    /// Upper bound on model↔tool iterations within one turn (loop-guard).
    pub const MAX_AGENTIC_STEPS: usize = 8;

    /// Execute a single tool call inside the sandbox and return its captured
    /// output. The command is taken from the `command` argument (a JSON array
    /// of strings); the result is persisted to memo as a `ToolResult` fragment
    /// so subsequent turns can recall it.
    pub async fn exec_tool_call(&self, call: &ToolCall) -> Result<ToolResult, CoreError> {
        sandbox_exec(&self.sandbox, &self.memo, &self.config.session, call).await
    }

    /// Run an agentic turn as an event stream: recall memo context → call the
    /// model (with `tools`) → execute any requested tool calls in the sandbox →
    /// refill memo → repeat until the model emits a final reply. The returned
    /// stream is `'static` and owns every dependency, so the [`Agent`] may be
    /// dropped while it is still consumed.
    ///
    /// Tool execution goes through the platform [`Sandbox`](agent_sandbox::Sandbox)
    /// seam (codex-backed per ADR-0005), never a bespoke in-process executor.
    pub async fn run_event_stream(
        &self,
        input: &str,
        tools: &[Tool],
    ) -> Result<BoxStream<'static, Result<AgentEvent, CoreError>>, CoreError> {
        let memo = self.memo.clone();
        let model = self.model.clone();
        let sandbox = self.sandbox.clone();
        let harness = self.harness.clone();
        let session = self.config.session.clone();
        let tools: Vec<Tool> = tools.to_vec();
        let input = input.to_string();

        // 1) recall context from memo (the ONLY context source).
        let fragments = memo.recall(&RecallQuery::new(&session, &input)).await?;
        let initial_context = fragments
            .iter()
            .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
            .collect::<Vec<_>>()
            .join("\n");
        // 2) persist the user turn.
        memo.memorize(ContextFragment::new(&session, FragmentKind::Message, input.clone()))
            .await?;

        let wrapped = async_stream::stream! {
            yield Ok(AgentEvent::Step { phase: "recall".into(), label: None });
            let mut tool_log = String::new();
            let mut step = 0usize;

            loop {
                step += 1;
                if step > Agent::MAX_AGENTIC_STEPS {
                    yield Ok(AgentEvent::Step {
                        phase: "loop_guard".into(),
                        label: Some("max agentic steps exceeded".into()),
                    });
                    yield Ok(AgentEvent::Done { text: String::new() });
                    break;
                }

                yield Ok(AgentEvent::Step { phase: "model".into(), label: None });
                let system = harness.get().system_text();
                let context = if tool_log.is_empty() {
                    initial_context.clone()
                } else {
                    format!("{}\n{}", initial_context, tool_log)
                };
                let req = ModelRequest {
                    system,
                    context,
                    input: input.clone(),
                };
                let turn = match model.complete_with_tools(&req, &tools).await {
                    Ok(t) => t,
                    Err(e) => {
                        yield Err(e);
                        return;
                    }
                };

                if turn.tool_calls.is_empty() {
                    // 3) final reply: stream the token then terminate.
                    yield Ok(AgentEvent::Token { text: turn.text.clone() });
                    let _ = memo
                        .memorize(ContextFragment::new(
                            &session,
                            FragmentKind::Message,
                            turn.text.clone(),
                        ))
                        .await;
                    yield Ok(AgentEvent::Done { text: turn.text });
                    break;
                }

                // 4) execute each requested tool call in the sandbox.
                for call in &turn.tool_calls {
                    yield Ok(AgentEvent::Step {
                        phase: "tool_exec".into(),
                        label: Some(call.name.clone()),
                    });
                    let result = match sandbox_exec(&sandbox, &memo, &session, call).await {
                        Ok(r) => r,
                        Err(e) => ToolResult {
                            call_id: call.id.clone(),
                            content: e.to_string(),
                            is_error: true,
                        },
                    };
                    tool_log.push_str(&format!(
                        "\n\nTOOL_RESULT[{}]: {}",
                        call.name, result.content
                    ));
                    yield Ok(AgentEvent::ToolCall {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                        result,
                    });
                }
            }
        };
        Ok(Box::pin(wrapped))
    }

    /// Run an agentic turn and fold the event stream into the final text.
    pub async fn run_agentic(
        &self,
        input: &str,
        tools: &[Tool],
    ) -> Result<String, CoreError> {
        let stream = self.run_event_stream(input, tools).await?;
        let mut out = String::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done { text } = ev? {
                out = text;
                break;
            }
        }
        Ok(out)
    }
}

/// Execute a tool call in the sandbox and persist the result to memo. Shared by
/// the agentic loop so it can be awaited without borrowing the [`Agent`].
async fn sandbox_exec(
    sandbox: &Arc<dyn Sandbox>,
    memo: &Arc<dyn MemoStore>,
    session: &str,
    call: &ToolCall,
) -> Result<ToolResult, CoreError> {
    let command: Vec<String> = call
        .arguments
        .get("command")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .ok_or_else(|| {
            CoreError::Model(format!("tool `{}` missing `command` array arg", call.name))
        })?;
    if command.is_empty() {
        return Err(CoreError::Model(format!(
            "tool `{}` command is empty",
            call.name
        )));
    }
    let spec = agent_sandbox::ExecSpec::command(command.clone());
    let handle = sandbox.spawn(&spec).await?;
    let out = sandbox.exec(&handle, &command).await?;
    sandbox.destroy(handle).await?;
    let content = format!(
        "exit={} stdout={} stderr={}",
        out.exit_code, out.stdout, out.stderr
    );
    memo.memorize(ContextFragment::new(
        session,
        FragmentKind::ToolResult,
        content.clone(),
    ))
    .await?;
    Ok(ToolResult {
        call_id: call.id.clone(),
        content,
        is_error: false,
    })
}

/// Convenience: a memory-backed memo store for quick local use.
pub fn in_memory_memo() -> Arc<dyn MemoStore> {
    SledMemoStore::memory().expect("sled temp store")
}

/// Re-export the default sandbox constructor for callers that don't need config.
pub fn default_sandbox_box() -> Box<dyn Sandbox> {
    default_sandbox()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_injects_memo_and_persists() {
        let memo = in_memory_memo();
        let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
        let agent = Agent::with_model(AgentConfig::default(), model, memo.clone()).unwrap();
        let r1 = agent.run("hello").await.unwrap();
        assert!(r1.contains("hello"));
        // second turn should recall the first
        let _ = agent.run("recap").await.unwrap();
        let frags = memo
            .recall(&RecallQuery::new("default", "hello"))
            .await
            .unwrap();
        assert!(frags.iter().any(|f| f.content == "hello"));
    }

    #[tokio::test]
    async fn exec_tool_runs_in_sandbox() {
        let memo = in_memory_memo();
        let agent = Agent::new(AgentConfig::default(), memo).unwrap();
        // Works only if `docker` is available; otherwise it errors gracefully.
        match agent.exec_tool(&["echo".into(), "hi".into()]).await {
            Ok(out) => assert!(out.contains("hi")),
            Err(_) => { /* docker / sandbox / memo not available in this environment */ }
        }
    }

    #[tokio::test]
    async fn run_stream_emits_tokens_and_persists() {
        let memo = in_memory_memo();
        let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
        let agent = Agent::with_model(AgentConfig::default(), model, memo.clone()).unwrap();
        let stream = agent.run_stream("hello").await.unwrap();
        let mut collected = String::new();
        let mut s = stream;
        while let Some(tok) = s.next().await {
            collected.push_str(&tok.unwrap());
        }
        assert!(collected.contains("hello"));
        // Both the user turn and the assistant reply are persisted to memo.
        let frags = memo
            .recall(&RecallQuery::new("default", "hello"))
            .await
            .unwrap();
        assert!(frags.iter().any(|f| f.content == "hello"));
    }

    #[tokio::test]
    async fn run_second_turn_injects_prior_context() {
        let memo = in_memory_memo();
        let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
        let agent = Agent::with_model(AgentConfig::default(), model, memo.clone()).unwrap();
        let _ = agent.run("remember the secret code 1234").await.unwrap();
        let second = agent.run("what was the code?").await.unwrap();
        // The stub emits `<injected>` only when recalled context is non-empty.
        assert!(second.contains("<injected>"));
    }

    #[tokio::test]
    async fn unknown_sandbox_provider_is_config_error() {
        let cfg = AgentConfig {
            sandbox_provider: "bogus".into(),
            ..AgentConfig::default()
        };
        let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
        let res = Agent::with_model(cfg, model, in_memory_memo());
        assert!(matches!(res, Err(CoreError::Config(_))));
    }

    #[test]
    fn core_error_converts_from_memo() {
        let e: CoreError = agent_memo::MemoError::NotFound("x".into()).into();
        assert!(matches!(e, CoreError::Memo(_)));
    }

    // --- Harness / ActiveHarness unit tests ---

    #[test]
    fn harness_baseline_equals_legacy_system() {
        let h = Harness::baseline("helper");
        assert_eq!(h.system_text(), "You are helper.");
        assert!(h.skills.is_empty());
        assert!(h.rules.is_empty());
    }

    #[test]
    fn harness_system_text_assembles_skills_and_rules() {
        let h = Harness {
            system_prompt: "You are a bot.".into(),
            skills: vec![Skill {
                name: "summarize".into(),
                body: "condense text".into(),
            }],
            rules: vec![Rule {
                name: "no_pii".into(),
                body: "never echo secrets".into(),
            }],
        };
        let s = h.system_text();
        assert!(s.contains("You are a bot."));
        assert!(s.contains("## Skills"));
        assert!(s.contains("summarize: condense text"));
        assert!(s.contains("## Rules"));
        assert!(s.contains("no_pii: never echo secrets"));
    }

    #[test]
    fn harness_serialization_roundtrip() {
        let h = Harness {
            system_prompt: "sys".into(),
            skills: vec![Skill {
                name: "s".into(),
                body: "b".into(),
            }],
            rules: vec![],
        };
        let json = serde_json::to_string(&h).unwrap();
        let back: Harness = serde_json::from_str(&json).unwrap();
        assert_eq!(h, back);
    }

    #[test]
    fn active_harness_hot_swap_is_atomic() {
        let ah = ActiveHarness::baseline("agent");
        assert!(ah.get().is_baseline("agent"));
        // Cloning the Arc is O(1) and shares the same harness.
        let snap = ah.get();
        ah.set(Harness::baseline("renamed"));
        // The old snapshot is unaffected, but a fresh get() sees the new value.
        assert!(snap.is_baseline("agent"));
        assert!(ah.get().is_baseline("renamed"));
    }

    /// Test model that echoes the system prompt so we can assert which harness
    /// was served on each turn.
    struct EchoModel;

    #[async_trait]
    impl ModelClient for EchoModel {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: format!("SYSTEM[{}]", req.system),
            })
        }
    }

    #[tokio::test]
    async fn run_uses_active_harness_and_hot_swaps() {
        let memo = in_memory_memo();
        let model: Box<dyn ModelClient> = Box::new(EchoModel);
        let harness = Arc::new(ActiveHarness::baseline("agent"));
        let agent =
            Agent::with_harness(AgentConfig::default(), model, memo, harness.clone()).unwrap();

        let r1 = agent.run("hi").await.unwrap();
        assert!(
            r1.contains("You are agent."),
            "baseline system served: {r1}"
        );

        // Hot-swap the harness; the next turn must use the new system prompt.
        harness.set(Harness {
            system_prompt: "Be terse.".into(),
            skills: vec![Skill {
                name: "short".into(),
                body: "reply in one line".into(),
            }],
            rules: vec![],
        });
        let r2 = agent.run("hi").await.unwrap();
        assert!(r2.contains("Be terse."), "swapped system served: {r2}");
        assert!(
            r2.contains("reply in one line"),
            "swapped skill served: {r2}"
        );
    }

    #[tokio::test]
    async fn with_model_builds_baseline_harness() {
        let memo = in_memory_memo();
        let model: Box<dyn ModelClient> = Box::new(EchoModel);
        let agent = Agent::with_model(AgentConfig::default(), model, memo).unwrap();
        let r = agent.run("hi").await.unwrap();
        assert!(r.contains("You are agent."));
    }

    // --- AgentEvent / agentic loop tests ---

    use agent_sandbox::{ExecSpec, ExecOutput, SandboxError, SandboxHandle};
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// A `Sandbox` that runs commands locally (no Docker) so tool-execution
    /// tests are hermetic and fast.
    struct LocalSandbox;

    #[async_trait]
    impl Sandbox for LocalSandbox {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Ok(SandboxHandle { id: "local".into() })
        }
        async fn exec(
            &self,
            _handle: &SandboxHandle,
            cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            let joined = cmd.join(" ");
            let out = tokio::process::Command::new("sh")
                .args(["-c", &joined])
                .output()
                .await
                .map_err(SandboxError::Io)?;
            Ok(ExecOutput {
                exit_code: out.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&out.stdout).to_string(),
                stderr: String::from_utf8_lossy(&out.stderr).to_string(),
            })
        }
        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    fn local_agent(model: Box<dyn ModelClient>) -> Agent {
        let harness = Arc::new(ActiveHarness::baseline("agent"));
        Agent::with_sandbox(
            AgentConfig::default(),
            model,
            in_memory_memo(),
            harness,
            Arc::new(LocalSandbox),
        )
        .unwrap()
    }

    #[test]
    fn agent_event_serde_uses_type_tag() {
        let tok = AgentEvent::Token { text: "hi".into() };
        let j = serde_json::to_string(&tok).unwrap();
        assert!(j.contains("\"type\":\"token\""));
        assert_eq!(serde_json::from_str::<AgentEvent>(&j).unwrap(), tok);

        let tc = AgentEvent::ToolCall {
            id: "c".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
            result: ToolResult {
                call_id: "c".into(),
                content: "x".into(),
                is_error: false,
            },
        };
        let j2 = serde_json::to_string(&tc).unwrap();
        assert!(j2.contains("\"type\":\"tool_call\""));
        assert_eq!(serde_json::from_str::<AgentEvent>(&j2).unwrap(), tc);

        let step = AgentEvent::Step {
            phase: "recall".into(),
            label: None,
        };
        assert!(serde_json::to_string(&step)
            .unwrap()
            .contains("\"type\":\"step\""));
        assert!(serde_json::to_string(&AgentEvent::Done { text: "x".into() })
            .unwrap()
            .contains("\"type\":\"done\""));
    }

    #[tokio::test]
    async fn stub_model_complete_with_tools_has_no_calls() {
        let model = StubModel::new("agent");
        let req = ModelRequest {
            system: "s".into(),
            context: String::new(),
            input: "hi".into(),
        };
        let turn = model.complete_with_tools(&req, &[]).await.unwrap();
        assert!(turn.tool_calls.is_empty());
        assert!(turn.text.contains("hi"));
    }

    #[tokio::test]
    async fn run_event_stream_single_shot_emits_events() {
        let memo = in_memory_memo();
        let agent = Agent::with_model(
            AgentConfig::default(),
            Box::new(StubModel::new("agent")),
            memo.clone(),
        )
        .unwrap();
        let stream = agent.run_event_stream("hello", &[]).await.unwrap();
        let mut events = Vec::new();
        let mut stream = stream;
        while let Some(ev) = stream.next().await {
            events.push(ev.unwrap());
        }
        assert!(
            matches!(events.first(), Some(AgentEvent::Step { phase, .. }) if phase == "recall"),
            "first event must be the recall step"
        );
        assert!(events.iter().any(|e| matches!(e, AgentEvent::Token { .. })));
        assert!(
            matches!(events.last(), Some(AgentEvent::Done { .. })),
            "stream must terminate with Done"
        );
        // Both turns persisted to memo.
        let frags = memo.recall(&RecallQuery::new("default", "hello")).await.unwrap();
        assert!(frags.iter().any(|f| f.content == "hello"));
    }

    #[tokio::test]
    async fn run_agentic_executes_tool_and_refills_memo() {
        let agent = local_agent(Box::new(ToolLoopModel {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let tools = vec![Tool {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: serde_json::json!({}),
        }];
        let stream = agent.run_event_stream("do it", &tools).await.unwrap();
        let mut stream = stream;
        let mut results = Vec::new();
        while let Some(ev) = stream.next().await {
            if let AgentEvent::ToolCall { result, .. } = ev.unwrap() {
                results.push(result);
            }
        }
        assert_eq!(results.len(), 1, "exactly one tool call executed");
        assert!(results[0].content.contains("hello"));
        assert!(!results[0].is_error);
        // The tool result was persisted to memo and can be recalled.
        let frags = agent
            .memo()
            .recall(&RecallQuery::new("default", "hello"))
            .await
            .unwrap();
        assert!(frags.iter().any(|f| f.content.contains("hello")));
    }

    #[tokio::test]
    async fn run_agentic_records_tool_failure() {
        let agent = local_agent(Box::new(MissingCommandModel {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let tools = vec![Tool {
            name: "shell".into(),
            description: "x".into(),
            parameters: serde_json::json!({}),
        }];
        let stream = agent.run_event_stream("fail", &tools).await.unwrap();
        let mut stream = stream;
        let mut saw_error = false;
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev.unwrap() {
                AgentEvent::ToolCall { result, .. } => saw_error = saw_error || result.is_error,
                AgentEvent::Done { .. } => done = true,
                _ => {}
            }
        }
        assert!(saw_error, "missing command must surface as an error tool result");
        assert!(done);
    }

    #[tokio::test]
    async fn run_agentic_respects_loop_cap() {
        let agent = local_agent(Box::new(LoopForeverModel));
        let tools = vec![Tool {
            name: "shell".into(),
            description: "x".into(),
            parameters: serde_json::json!({}),
        }];
        let stream = agent.run_event_stream("loop", &tools).await.unwrap();
        let mut stream = stream;
        let mut model_steps = 0usize;
        let mut done = false;
        while let Some(ev) = stream.next().await {
            match ev.unwrap() {
                AgentEvent::Step { phase, .. } if phase == "model" => model_steps += 1,
                AgentEvent::Done { .. } => done = true,
                _ => {}
            }
        }
        assert!(done, "must terminate with a Done event even when looping");
        assert!(
            model_steps <= Agent::MAX_AGENTIC_STEPS,
            "model steps bounded by cap, got {model_steps}"
        );
    }

    #[tokio::test]
    async fn exec_tool_call_runs_in_sandbox_and_persists() {
        let memo = in_memory_memo();
        let agent = Agent::with_sandbox(
            AgentConfig::default(),
            Box::new(StubModel::new("agent")),
            memo.clone(),
            Arc::new(ActiveHarness::baseline("agent")),
            Arc::new(LocalSandbox),
        )
        .unwrap();
        let call = ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({ "command": ["echo", "hi"] }),
        };
        let res = agent.exec_tool_call(&call).await.unwrap();
        assert!(res.content.contains("hi"));
        assert!(!res.is_error);
        let frags = memo.recall(&RecallQuery::new("default", "hi")).await.unwrap();
        assert!(frags.iter().any(|f| f.content.contains("hi")));
    }

    #[tokio::test]
    async fn exec_tool_call_missing_command_errors() {
        let agent = local_agent(Box::new(StubModel::new("agent")));
        let call = ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            arguments: serde_json::json!({}),
        };
        let res = agent.exec_tool_call(&call).await;
        assert!(matches!(res, Err(CoreError::Model(_))));
    }

    /// Returns a tool call on the first turn, then a final reply afterwards.
    struct ToolLoopModel {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelClient for ToolLoopModel {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: format!("[stub] {}", req.input),
            })
        }
        async fn complete_with_tools(
            &self,
            req: &ModelRequest,
            _tools: &[Tool],
        ) -> Result<ModelTurn, CoreError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(ModelTurn {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({ "command": ["echo", "hello"] }),
                    }],
                })
            } else {
                Ok(ModelTurn {
                    text: format!("final reply for: {}", req.input),
                    tool_calls: vec![],
                })
            }
        }
    }

    /// Returns a tool call with no `command` arg first, then a final reply.
    struct MissingCommandModel {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ModelClient for MissingCommandModel {
        async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: String::new(),
            })
        }
        async fn complete_with_tools(
            &self,
            _req: &ModelRequest,
            _tools: &[Tool],
        ) -> Result<ModelTurn, CoreError> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                Ok(ModelTurn {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "bad".into(),
                        name: "shell".into(),
                        arguments: serde_json::json!({}),
                    }],
                })
            } else {
                Ok(ModelTurn {
                    text: "recovered".into(),
                    tool_calls: vec![],
                })
            }
        }
    }

    /// Always asks for the same tool call, to exercise the loop cap.
    struct LoopForeverModel;

    #[async_trait]
    impl ModelClient for LoopForeverModel {
        async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: String::new(),
            })
        }
        async fn complete_with_tools(
            &self,
            _req: &ModelRequest,
            _tools: &[Tool],
        ) -> Result<ModelTurn, CoreError> {
            Ok(ModelTurn {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "c".into(),
                    name: "shell".into(),
                    arguments: serde_json::json!({ "command": ["echo", "x"] }),
                }],
            })
        }
    }
}
