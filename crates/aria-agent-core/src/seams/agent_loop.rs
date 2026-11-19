//! Agent-loop seam (`ctx.agentLoop`).
//!
//! dsh calls `ctx.agentLoop` its only *bundle*: Cordis composes one loop out of
//! the other services. We assemble statically, so an interchangeable runtime
//! (native Rust loop vs. an external harness such as DeepSeek Harness) only
//! works if the loop is a **contract** — hence a seam here (ADR-0019 §deviations).
//!
//! The contract is deliberately narrow:
//!
//! * [`LoopContext`] is a snapshot of everything a loop may touch, resolved once
//!   at assembly so the hot path never touches a registry (ADR-0014).
//! * The only output vocabulary is [`AgentEvent`]; implementations must not add
//!   variants. The cloud's `event_envelope` is the single, frozen translation
//!   point to the OpenAI beta Agents frames (ADR-0009).
//! * Persistence is *not* optional: an implementation must mirror the same
//!   [`SessionEvent`](crate::session::SessionEvent) sequence the native loop
//!   writes, so `sessionQuery` / titles / turns stay backend-agnostic.
//!
//! Cancellation: dropping the returned stream stops further work. Implementations
//! that own external resources must release them on drop of the stream (or of the
//! context they took ownership of).

use async_trait::async_trait;
use futures::stream::BoxStream;
use std::sync::Arc;

use crate::context::ContextStore;
use crate::prompt::SystemPrompt;
use crate::seams::ToolExecutionPipeline;
use crate::session::{SessionPersistence, SessionTitleProvider};
use crate::{AgentConfig, AgentEvent, CoreError, ModelClient, Tool};
use agent_sandbox::Sandbox;

/// Everything one turn may touch, resolved once at assembly time.
#[derive(Clone)]
pub struct LoopContext {
    pub config: AgentConfig,
    pub model: Arc<dyn ModelClient>,
    pub context: Arc<dyn ContextStore>,
    pub sandbox: Arc<dyn Sandbox>,
    pub pipeline: Arc<ToolExecutionPipeline>,
    pub prompt: SystemPrompt,
    pub session_log: Option<Arc<dyn SessionPersistence>>,
    pub tools: Vec<Tool>,
    pub title_pending: Arc<std::sync::atomic::AtomicBool>,
    pub title_provider: Arc<dyn SessionTitleProvider>,
}

impl LoopContext {
    /// The assembled system prompt for this turn.
    pub fn system_text(&self) -> String {
        self.prompt.build()
    }

    /// The session this turn belongs to.
    pub fn session(&self) -> &str {
        &self.config.session
    }
}

/// Drives one conversational turn.
#[async_trait]
pub trait AgentLoop: Send + Sync {
    /// Run one agentic turn and stream its events.
    ///
    /// The returned stream is `'static` and owns everything it needs, so the
    /// [`Agent`](crate::Agent) may be dropped while it is still consumed.
    async fn run(
        &self,
        ctx: LoopContext,
        input: &str,
    ) -> Result<BoxStream<'static, Result<AgentEvent, CoreError>>, CoreError>;

    /// One-shot turn without tools (the `POST …/turns` and FFI `run` path).
    ///
    /// The default folds [`Self::run`] and keeps the last `Done` text, which is
    /// what an external harness wants: the harness decides whether the turn is
    /// agentic. Providers that have a cheaper single-call path override it.
    async fn run_once(&self, ctx: LoopContext, input: &str) -> Result<String, CoreError> {
        let stream = self.run(ctx, input).await?;
        let mut stream = stream;
        let mut out = String::new();
        while let Some(ev) = futures::StreamExt::next(&mut stream).await {
            if let AgentEvent::Done { text } = ev? {
                out = text;
                break;
            }
        }
        Ok(out)
    }
}
