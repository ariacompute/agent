//! `agent-core` — the unified agent runtime.
//!
//! It wraps the codex harness-style agent loop and guarantees the **memo
//! contract**: every [`Agent::run`] first [`recall`](agent_memo::MemoStore::recall)s
//! context from memo, then after producing a reply [`memorize`](agent_memo::MemoStore::memorize)s
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
use std::sync::Arc;
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
        Ok(Box::pin(futures::stream::once(async move { Ok(resp.text) })))
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

/// The unified agent. Holds the model client, memo store, and sandbox.
pub struct Agent {
    config: AgentConfig,
    model: Box<dyn ModelClient>,
    memo: Arc<dyn MemoStore>,
    sandbox: Box<dyn Sandbox>,
}

impl Agent {
    /// Build with an explicit model client (e.g. [`StubModel`] or [`OpenAiModel`]).
    pub fn with_model(
        config: AgentConfig,
        model: Box<dyn ModelClient>,
        memo: Arc<dyn MemoStore>,
    ) -> Result<Self, CoreError> {
        let provider = SandboxProvider::parse(&config.sandbox_provider).ok_or_else(|| {
            CoreError::Config(format!("unknown sandbox: {}", config.sandbox_provider))
        })?;
        let sandbox = agent_sandbox::from_provider(provider);
        Ok(Self {
            config,
            model,
            memo,
            sandbox,
        })
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
        let req = ModelRequest {
            system: format!("You are {}.", self.config.agent_name),
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
        let req = ModelRequest {
            system: format!("You are {}.", self.config.agent_name),
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
}
