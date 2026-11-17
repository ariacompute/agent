//! The agent loop.
//!
//! One turn is: `TurnStarted` → recall (+ optional graph expansion) → loop {
//! pressure check → `llm` → tool pipeline → events } → `TurnCompleted` /
//! `TurnFailed`. Every event is mirrored into the append-only
//! [`SessionRecorder`], which flushes to `sessionPersistence` at turn boundaries
//! (ADR-0015). Tool calls go through the fixed pipeline
//! (preset → approval → dispatch → post-exec) from ADR-0016.

use crate::context::{
    ContextFragment, ContextStore, FragmentKind, GraphRetrieveQuery, RecallQuery, RelationKind,
};
use crate::seams::{Authz, ToolExecutionPipeline};
use crate::session::{
    CompactDecision, CompactReason, CompactionPolicy, SessionEvent, SessionPersistence,
    SessionRecorder, SessionTitleProvider,
};
use crate::token_meter::TokenMeter;
use crate::{
    graph_relate_store, maybe_compact_store, sandbox_exec, Agent, AgentConfig, AgentEvent,
    CoreError, ModelRequest, Tool, ToolCall, ToolHandler, ToolResult,
};
use agent_sandbox::Sandbox;
use futures::stream::{BoxStream, StreamExt};
use serde_json::json;
use std::sync::Arc;

/// Render recalled fragments into the prompt context block.
///
/// One formatting function so the first attempt and an overflow-triggered retry
/// hand the model the same shape.
fn render_fragments(fragments: &[ContextFragment]) -> String {
    fragments
        .iter()
        .map(|f| format!("[{}] {}", f.kind.as_str(), f.content))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Record the opening events of a turn: `turn_started`, then the deterministic
/// title (once per agent instance) plus an optional provider override.
///
/// Every entry point shares this — the event stream, the one-shot [`Agent::run`]
/// and [`Agent::run_stream`]. That is how a delegated child, whose provider calls
/// `run`, ends up with a `title_set` record and therefore a row in
/// `session_titles` (ADR-0015, ADR-0017).
async fn record_turn_open(
    recorder: &mut SessionRecorder,
    title_pending: &std::sync::atomic::AtomicBool,
    title_provider: &Arc<dyn SessionTitleProvider>,
    session: &str,
    turn_id: &str,
    input: &str,
) {
    recorder.push(SessionEvent::TurnStarted {
        turn_id: turn_id.to_string(),
        input: input.to_string(),
    });
    if title_pending.swap(false, std::sync::atomic::Ordering::SeqCst) {
        recorder.push(SessionEvent::TitleSet {
            title: crate::session::first_prompt_title(input),
            source: crate::session::SOURCE_FIRST_PROMPT.into(),
        });
        if let Some(title) = title_provider.title(session, input).await {
            let title = title.trim();
            if !title.is_empty() {
                recorder.push(SessionEvent::TitleSet {
                    title: title.to_string(),
                    source: crate::session::SOURCE_PROVIDER.into(),
                });
            }
        }
    }
}

/// Close a failed turn on a shared recorder (used by the streaming paths, whose
/// recorder is behind an async mutex because the stream outlives the call).
async fn fail_turn(
    recorder: &Arc<tokio::sync::Mutex<SessionRecorder>>,
    turn_id: &str,
    error: &CoreError,
) {
    let mut guard = recorder.lock().await;
    guard.push(SessionEvent::TurnFailed {
        turn_id: turn_id.to_string(),
        message: error.to_string(),
    });
    let _ = guard.flush().await;
}

impl Agent {
    /// The effective configuration.
    /// A recorder bound to this agent's session and persistence backend.
    fn recorder(&self) -> SessionRecorder {
        match &self.session_log {
            Some(log) => {
                SessionRecorder::with_persistence(self.config.session.clone(), log.clone())
            }
            None => SessionRecorder::new(self.config.session.clone()),
        }
    }

    pub fn config(&self) -> &AgentConfig {
        &self.config
    }

    pub fn session(&self) -> &str {
        &self.config.session
    }

    /// The assembled system prompt.
    pub fn system_text(&self) -> String {
        self.prompt.build()
    }

    /// Static prompt sections (callers may clone + append skills/plan).
    pub fn prompt(&self) -> &crate::prompt::SystemPrompt {
        &self.prompt
    }

    /// Shared context store (used by the SDK to expose `Session`).
    pub fn context(&self) -> Arc<dyn ContextStore> {
        self.context.clone()
    }

    /// The fixed tool pipeline.
    pub fn pipeline(&self) -> &ToolExecutionPipeline {
        &self.pipeline
    }

    /// Registered tool handlers (in consultation order).
    pub fn handlers(&self) -> &[Arc<dyn ToolHandler>] {
        &self.handlers
    }

    /// Execution backend.
    pub fn sandbox(&self) -> &Arc<dyn Sandbox> {
        &self.sandbox
    }

    /// Session event persistence, when configured.
    pub fn session_log(&self) -> Option<Arc<dyn SessionPersistence>> {
        self.session_log.clone()
    }

    /// Supplement vector recall with bounded graph expansion from `seeds`.
    async fn graph_expand(&self, seeds: Vec<String>, top_k: usize) -> Vec<ContextFragment> {
        if !self.config.memory_graph || seeds.is_empty() {
            return Vec::new();
        }
        match self
            .context
            .expand(&GraphRetrieveQuery {
                session: self.config.session.clone(),
                seeds,
                views: Vec::new(),
                budget: 32,
                max_hops: 2,
                top_k,
            })
            .await
        {
            Ok(res) => res.items.into_iter().map(|s| s.fragment).collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Recall + graph expansion, rendered as prompt context.
    async fn recall_context(&self, input: &str) -> Result<String, CoreError> {
        let session = &self.config.session;
        let mut fragments = self
            .context
            .recall(&RecallQuery::new(session, input))
            .await?;
        let extra = self
            .graph_expand(fragments.iter().map(|f| f.id.clone()).collect(), 8)
            .await;
        for f in extra {
            if !fragments.iter().any(|x| x.id == f.id) {
                fragments.push(f);
            }
        }
        Ok(render_fragments(&fragments))
    }

    /// Run one turn. Injects recalled context, calls the model, persists both turns.
    ///
    /// The turn is recorded exactly like the streaming loop: `turn_started`, the
    /// title (flushed immediately, so a slow or failing model cannot lose it) and
    /// a terminal `turn_completed` / `turn_failed`.
    pub async fn run(&self, input: &str) -> Result<String, CoreError> {
        let session = self.config.session.clone();
        let mut recorder = self.recorder();
        let turn_id = uuid::Uuid::new_v4().to_string();
        record_turn_open(
            &mut recorder,
            &self.title_pending,
            &self.title_provider,
            &session,
            &turn_id,
            input,
        )
        .await;
        // Flush before calling the model: a title must survive a provider that
        // hangs or fails.
        let _ = recorder.flush().await;

        let context = self.recall_context(input).await?;

        let user = ContextFragment::new(&session, FragmentKind::Message, input);
        let user_id = user.id.clone();
        self.context.memorize(user).await?;

        let req = ModelRequest {
            system: self.system_text(),
            context,
            input: input.to_string(),
        };
        let resp = match self.model.complete(&req).await {
            Ok(resp) => resp,
            Err(e) => {
                recorder.push(SessionEvent::TurnFailed {
                    turn_id,
                    message: e.to_string(),
                });
                let _ = recorder.flush().await;
                return Err(e);
            }
        };

        let asst = ContextFragment::new(&session, FragmentKind::Message, resp.text.clone());
        let asst_id = asst.id.clone();
        self.context.memorize(asst).await?;
        graph_relate_store(
            &self.context,
            &session,
            &user_id,
            &asst_id,
            RelationKind::Temporal,
        )
        .await;
        maybe_compact_store(
            &self.context,
            &session,
            self.config.memory_compact_threshold,
        )
        .await;

        recorder.push(SessionEvent::TextDone {
            text: resp.text.clone(),
        });
        recorder.push(SessionEvent::TurnCompleted {
            turn_id,
            output: resp.text.clone(),
        });
        let _ = recorder.flush().await;
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
        self.context
            .memorize(ContextFragment::new(
                &self.config.session,
                FragmentKind::ToolResult,
                captured.clone(),
            ))
            .await?;
        Ok(captured)
    }

    /// Stream one turn token-by-token (no tool calls).
    pub async fn run_stream(
        &self,
        input: &str,
    ) -> Result<BoxStream<'static, Result<String, CoreError>>, CoreError> {
        let session = self.config.session.clone();
        // Record the turn opening before anything can fail, and flush the title
        // right away: a stream that never completes must still leave a titled,
        // addressable session behind.
        let mut recorder = self.recorder();
        let turn_id = uuid::Uuid::new_v4().to_string();
        record_turn_open(
            &mut recorder,
            &self.title_pending,
            &self.title_provider,
            &session,
            &turn_id,
            input,
        )
        .await;
        let _ = recorder.flush().await;
        let recorder = Arc::new(tokio::sync::Mutex::new(recorder));

        let context = match self.recall_context(input).await {
            Ok(context) => context,
            Err(e) => {
                fail_turn(&recorder, &turn_id, &e).await;
                return Err(e);
            }
        };

        let user = ContextFragment::new(&session, FragmentKind::Message, input);
        let user_id = user.id.clone();
        if let Err(e) = self
            .context
            .memorize(user)
            .await
            .map_err(CoreError::Context)
        {
            fail_turn(&recorder, &turn_id, &e).await;
            return Err(e);
        }

        let req = ModelRequest {
            system: self.system_text(),
            context,
            input: input.to_string(),
        };
        let upstream = match self.model.stream(&req).await {
            Ok(upstream) => upstream,
            Err(e) => {
                fail_turn(&recorder, &turn_id, &e).await;
                return Err(e);
            }
        };

        let memo = self.context.clone();
        let graph_enabled = self.config.memory_graph;
        let threshold = self.config.memory_compact_threshold;
        let session_for_relate = session.clone();
        let rec_for_stream = recorder.clone();
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
                        fail_turn(&rec_for_stream, &turn_id, &e).await;
                        yield Err(e);
                        return;
                    }
                }
            }
            let asst = ContextFragment::new(
                &session_for_relate,
                FragmentKind::Message,
                collected.clone(),
            );
            let asst_id = asst.id.clone();
            let _ = memo.memorize(asst).await;
            if graph_enabled {
                graph_relate_store(
                    &memo,
                    &session_for_relate,
                    &user_id,
                    &asst_id,
                    RelationKind::Temporal,
                )
                .await;
                maybe_compact_store(&memo, &session_for_relate, threshold).await;
            }
            let mut guard = rec_for_stream.lock().await;
            guard.push(SessionEvent::TextDone {
                text: collected.clone(),
            });
            guard.push(SessionEvent::TurnCompleted {
                turn_id,
                output: collected,
            });
            let _ = guard.flush().await;
        };
        Ok(Box::pin(wrapped))
    }

    /// Upper bound on model↔tool iterations within one turn (loop-guard).
    pub const MAX_AGENTIC_STEPS: usize = 8;

    /// Execute a single tool call through the fixed pipeline (authorize →
    /// dispatch/fallback → post-exec) and return its result.
    pub async fn exec_tool_call(&self, call: &ToolCall) -> Result<ToolResult, CoreError> {
        let session = self.config.session.as_str();
        let (result, _approval) = self.run_pipeline(call, session).await;
        Ok(result)
    }

    /// Authorize + execute one call. Returns the result and the approval decision
    /// (if one was consulted) so the loop can emit an event.
    async fn run_pipeline(
        &self,
        call: &ToolCall,
        session: &str,
    ) -> (ToolResult, Option<crate::seams::ApprovalDecision>) {
        match self.pipeline.authorize(call, session).await {
            Authz::Denied { decision, reason } => (
                ToolExecutionPipeline::denied_result(call, &reason),
                Some(decision),
            ),
            Authz::Allowed { approval } => {
                let raw = match self.pipeline.dispatch(call, &self.context, session).await {
                    Some(result) => result,
                    None => match sandbox_exec(&self.sandbox, &self.context, session, call).await {
                        Ok(result) => result,
                        Err(e) => ToolResult {
                            call_id: call.id.clone(),
                            content: e.to_string(),
                            is_error: true,
                        },
                    },
                };
                let outcome = self.pipeline.post_execute(&call.id, raw, approval.clone());
                (outcome.result, approval)
            }
        }
    }

    /// Run an agentic turn as an event stream.
    ///
    /// The returned stream is `'static` and owns every dependency, so the
    /// [`Agent`] may be dropped while it is still consumed. Events are mirrored
    /// into the session recorder and flushed when the turn ends.
    pub async fn run_event_stream(
        &self,
        input: &str,
        tools: &[Tool],
    ) -> Result<BoxStream<'static, Result<AgentEvent, CoreError>>, CoreError> {
        let memo = self.context.clone();
        let model = self.model.clone();
        let sandbox = self.sandbox.clone();
        let pipeline = self.pipeline.clone();
        let system = self.system_text();
        let session = self.config.session.clone();
        let model_name = self.config.model.clone();
        let tools: Vec<Tool> = tools.to_vec();
        let input_owned = input.to_string();
        let graph_enabled = self.config.memory_graph;
        let threshold = self.config.memory_compact_threshold;
        let context_limit = self.config.context_limit;
        let title_pending = self.title_pending.clone();
        let title_provider = self.title_provider.clone();
        let mut recorder = self.recorder();

        // 1) recall context from memo (+ optional graph expansion).
        let mut initial_context = self.recall_context(&input_owned).await?;
        // 2) persist the user turn.
        let user = ContextFragment::new(&session, FragmentKind::Message, input_owned.clone());
        let user_id = user.id.clone();
        memo.memorize(user).await?;

        let wrapped = async_stream::stream! {
            let mut meter = TokenMeter::new(context_limit, crate::token_meter::DEFAULT_PRESSURE_RATIO);
            let turn_id = uuid::Uuid::new_v4().to_string();
            // Deterministic first-prompt title plus an optional provider
            // override (ADR-0015); shared with `run` / `run_stream` so a session
            // is titled no matter which loop drives it.
            record_turn_open(
                &mut recorder,
                &title_pending,
                &title_provider,
                &session,
                &turn_id,
                &input_owned,
            )
            .await;
            yield Ok(AgentEvent::Step { phase: "recall".into(), label: None });
            recorder.push(SessionEvent::Step { phase: "recall".into(), label: None });

            let mut tool_log = String::new();
            let mut step = 0usize;
            let mut overflow_retried = false;

            loop {
                step += 1;
                if step > Agent::MAX_AGENTIC_STEPS {
                    let label = "max agentic steps exceeded".to_string();
                    yield Ok(AgentEvent::Step {
                        phase: "loop_guard".into(),
                        label: Some(label.clone()),
                    });
                    recorder.push(SessionEvent::Step {
                        phase: "loop_guard".into(),
                        label: Some(label),
                    });
                    recorder.push(SessionEvent::TurnCompleted {
                        turn_id: turn_id.clone(),
                        output: String::new(),
                    });
                    let _ = recorder.flush().await;
                    yield Ok(AgentEvent::Done { text: String::new() });
                    break;
                }

                // Context pressure: compact before asking the model again.
                if meter.under_pressure() {
                    yield Ok(AgentEvent::Step { phase: "compact".into(), label: None });
                    if let Ok(frag) = memo.compact(&session).await {
                        meter.reset();
                        recorder.push(SessionEvent::Compacted {
                            fragment_id: frag.id.clone(),
                            removed: 0,
                            reason: "context pressure".into(),
                        });
                        yield Ok(AgentEvent::Step {
                            phase: "compact_done".into(),
                            label: None,
                        });
                    }
                }

                yield Ok(AgentEvent::Step { phase: "model".into(), label: None });
                recorder.push(SessionEvent::Step { phase: "model".into(), label: None });
                let context = if tool_log.is_empty() {
                    initial_context.clone()
                } else {
                    format!("{}\n{}", initial_context, tool_log)
                };
                let req = ModelRequest {
                    system: system.clone(),
                    context,
                    input: input_owned.clone(),
                };
                let turn = match model.complete_with_tools(&req, &tools).await {
                    Ok(t) => t,
                    Err(e) => {
                        let message = e.to_string();
                        // Provider reported a context overflow: compact once and
                        // retry with the shrunken context (ADR-0015). A second
                        // overflow is terminal so the loop cannot spin.
                        let recover = !overflow_retried
                            && matches!(
                                CompactionPolicy::default().after_error(Some(&message)),
                                CompactDecision::Compact(CompactReason::ContextOverflow)
                            );
                        if !recover {
                            recorder.push(SessionEvent::TurnFailed {
                                turn_id: turn_id.clone(),
                                message: message.clone(),
                            });
                            let _ = recorder.flush().await;
                            yield Err(CoreError::Model(message));
                            return;
                        }
                        overflow_retried = true;
                        yield Ok(AgentEvent::Step {
                            phase: "compact".into(),
                            label: Some(CompactReason::ContextOverflow.as_str().into()),
                        });
                        match memo.compact(&session).await {
                            Ok(frag) => {
                                meter.reset();
                                recorder.push(SessionEvent::Compacted {
                                    fragment_id: frag.id.clone(),
                                    removed: 0,
                                    reason: CompactReason::ContextOverflow.as_str().into(),
                                });
                                // Re-recall so the retry genuinely carries less
                                // context than the rejected request.
                                initial_context = match memo
                                    .recall(&RecallQuery::new(&session, &input_owned))
                                    .await
                                {
                                    Ok(frags) => render_fragments(&frags),
                                    Err(_) => String::new(),
                                };
                            }
                            Err(err) => {
                                let reason = format!("{message}; compaction failed: {err}");
                                recorder.push(SessionEvent::TurnFailed {
                                    turn_id: turn_id.clone(),
                                    message: reason.clone(),
                                });
                                let _ = recorder.flush().await;
                                yield Err(CoreError::Model(reason));
                                return;
                            }
                        }
                        yield Ok(AgentEvent::Step {
                            phase: "compact_done".into(),
                            label: None,
                        });
                        continue;
                    }
                };

                // Provider-reported usage wins; the local estimate is only the
                // fallback for backends that report nothing (ADR-0015).
                match &turn.usage {
                    Some(usage) => meter.observe(usage),
                    None => meter.observe_text(&req.input, &turn.text, &model_name),
                }
                let reported_model = turn
                    .usage
                    .as_ref()
                    .map(|u| u.model.clone())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| model_name.clone());
                yield Ok(AgentEvent::Usage {
                    input_tokens: meter.usage().input_tokens,
                    output_tokens: meter.usage().output_tokens,
                    model: reported_model.clone(),
                });
                recorder.push(SessionEvent::Usage {
                    input_tokens: meter.usage().input_tokens,
                    output_tokens: meter.usage().output_tokens,
                    model: reported_model,
                });

                if turn.tool_calls.is_empty() {
                    yield Ok(AgentEvent::Token { text: turn.text.clone() });
                    recorder.push(SessionEvent::TextDone { text: turn.text.clone() });
                    let asst = ContextFragment::new(
                        &session,
                        FragmentKind::Message,
                        turn.text.clone(),
                    );
                    let asst_id = asst.id.clone();
                    let _ = memo.memorize(asst).await;
                    if graph_enabled {
                        graph_relate_store(
                            &memo,
                            &session,
                            &user_id,
                            &asst_id,
                            RelationKind::Temporal,
                        )
                        .await;
                        maybe_compact_store(&memo, &session, threshold).await;
                    }
                    recorder.push(SessionEvent::TurnCompleted {
                        turn_id: turn_id.clone(),
                        output: turn.text.clone(),
                    });
                    let _ = recorder.flush().await;
                    yield Ok(AgentEvent::Done { text: turn.text });
                    break;
                }

                // 4) execute each requested tool call through the pipeline.
                for call in &turn.tool_calls {
                    yield Ok(AgentEvent::Step {
                        phase: "tool_exec".into(),
                        label: Some(call.name.clone()),
                    });
                    recorder.push(SessionEvent::ItemAdded {
                        item_id: call.id.clone(),
                        kind: "tool_call".into(),
                        payload: json!({
                            "name": call.name,
                            "arguments": call.arguments,
                        }),
                    });

                    let (result, _approval) = match pipeline.authorize(call, &session).await {
                        Authz::Denied { decision, reason } => {
                            yield Ok(AgentEvent::Approval {
                                id: call.id.clone(),
                                tool: call.name.clone(),
                                decision: decision.as_str().to_string(),
                                reason: Some(reason.clone()),
                            });
                            recorder.push(SessionEvent::Approval {
                                id: call.id.clone(),
                                tool: call.name.clone(),
                                decision: decision.as_str().to_string(),
                                reason: Some(reason.clone()),
                            });
                            (
                                ToolExecutionPipeline::denied_result(call, &reason),
                                Some(decision),
                            )
                        }
                        Authz::Allowed { approval } => {
                            if let Some(decision) = &approval {
                                yield Ok(AgentEvent::Approval {
                                    id: call.id.clone(),
                                    tool: call.name.clone(),
                                    decision: decision.as_str().to_string(),
                                    reason: None,
                                });
                                recorder.push(SessionEvent::Approval {
                                    id: call.id.clone(),
                                    tool: call.name.clone(),
                                    decision: decision.as_str().to_string(),
                                    reason: None,
                                });
                            }
                            let raw = match pipeline.dispatch(call, &memo, &session).await {
                                Some(result) => result,
                                None => {
                                    match sandbox_exec(&sandbox, &memo, &session, call).await {
                                        Ok(result) => result,
                                        Err(e) => ToolResult {
                                            call_id: call.id.clone(),
                                            content: e.to_string(),
                                            is_error: true,
                                        },
                                    }
                                }
                            };
                            let outcome = pipeline.post_execute(&call.id, raw, approval.clone());
                            (outcome.result, approval)
                        }
                    };

                    tool_log.push_str(&format!(
                        "\n\nTOOL_RESULT[{}]: {}",
                        call.name, result.content
                    ));
                    recorder.push(SessionEvent::ItemDone {
                        item_id: call.id.clone(),
                        result: json!({
                            "content": result.content,
                            "spill": result.content.len(),
                        }),
                        is_error: result.is_error,
                    });
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
    pub async fn run_agentic(&self, input: &str, tools: &[Tool]) -> Result<String, CoreError> {
        let stream = self.run_event_stream(input, tools).await?;
        let mut stream = stream;
        let mut out = String::new();
        while let Some(ev) = stream.next().await {
            if let AgentEvent::Done { text } = ev? {
                out = text;
                break;
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::PermissionPreset;
    use crate::session::{JsonlPersistence, SessionProjection, TurnStatus};
    use crate::{build_policy, in_memory_context, ModelClient, ModelResponse, ModelTurn};
    use agent_sandbox::{ExecOutput, ExecSpec, SandboxError, SandboxHandle};
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// `Sandbox` that records nothing and never touches Docker.
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
            Ok(ExecOutput {
                exit_code: 0,
                stdout: format!("ran {}", cmd.join(" ")),
                stderr: String::new(),
            })
        }

        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            Ok(())
        }
    }

    /// First model call asks for a `shell` tool, the second answers in text.
    #[derive(Default)]
    struct ToolThenText {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelClient for ToolThenText {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: format!("final answer for {}", req.input),
            })
        }

        async fn complete_with_tools(
            &self,
            _req: &ModelRequest,
            _tools: &[Tool],
        ) -> Result<ModelTurn, CoreError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(ModelTurn {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "shell".into(),
                        arguments: json!({"command": ["echo", "hi"]}),
                    }],
                    usage: Some(crate::seams::TokenUsage::new(11, 5, "stub")),
                })
            } else {
                Ok(ModelTurn::reply("final answer"))
            }
        }
    }

    fn shell_tools() -> Vec<Tool> {
        vec![Tool {
            name: "shell".into(),
            description: "run a shell command".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["command"]
            }),
        }]
    }

    fn agent_with_model(log_dir: &Path, model: Box<dyn ModelClient>) -> Agent {
        let root = log_dir.to_string_lossy().to_string();
        let cfg = AgentConfig {
            session: "s1".into(),
            agent_name: "tester".into(),
            workspace_root: root.clone(),
            session_log_dir: root,
            ..AgentConfig::default()
        };
        Agent::with_sandbox(cfg, model, in_memory_context(), Arc::new(LocalSandbox))
            .expect("agent builds")
    }

    fn agent(log_dir: &Path, approval_mode: &str, preset: &str) -> Agent {
        let root = log_dir.to_string_lossy().to_string();
        let cfg = AgentConfig {
            session: "s1".into(),
            agent_name: "tester".into(),
            approval_mode: approval_mode.into(),
            permission_preset: preset.into(),
            workspace_root: root.clone(),
            session_log_dir: root,
            ..AgentConfig::default()
        };
        Agent::with_sandbox(
            cfg,
            Box::new(ToolThenText::default()),
            in_memory_context(),
            Arc::new(LocalSandbox),
        )
        .expect("agent builds")
    }

    async fn collect(agent: &Agent, input: &str) -> Vec<AgentEvent> {
        let mut stream = agent
            .run_event_stream(input, &shell_tools())
            .await
            .expect("stream starts");
        let mut events = Vec::new();
        while let Some(ev) = stream.next().await {
            events.push(ev.expect("event"));
        }
        events
    }

    fn tool_result(events: &[AgentEvent]) -> ToolResult {
        events
            .iter()
            .find_map(|e| match e {
                AgentEvent::ToolCall { result, .. } => Some(result.clone()),
                _ => None,
            })
            .expect("one tool call event")
    }

    #[tokio::test]
    async fn denied_tool_call_is_refused_and_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path(), "deny", "workspace_write");

        // The preset allows mutation, but the fail-closed broker refuses it.
        let authz = agent
            .pipeline()
            .authorize(
                &ToolCall {
                    id: "c1".into(),
                    name: "shell".into(),
                    arguments: json!({"command": ["echo", "hi"]}),
                },
                "s1",
            )
            .await;
        assert!(matches!(authz, Authz::Denied { .. }), "{authz:?}");

        let events = collect(&agent, "hi").await;
        let (decision, reason) = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Approval {
                    decision, reason, ..
                } => Some((decision.clone(), reason.clone())),
                _ => None,
            })
            .expect("approval event");
        assert_eq!(decision, "unavailable");
        assert!(
            reason.unwrap_or_default().contains("no approval provider"),
            "reason must explain the refusal"
        );

        let result = tool_result(&events);
        assert!(result.is_error, "denied tool must not report success");
        assert!(result.content.contains("denied"), "{}", result.content);
        assert!(matches!(events.last(), Some(AgentEvent::Done { .. })));

        let records = JsonlPersistence::new(dir.path())
            .read("s1", 0)
            .await
            .expect("session log written");
        let kinds: Vec<&str> = records.iter().map(|r| r.event.kind()).collect();
        for want in [
            "turn_started",
            "title_set",
            "approval",
            "usage",
            "item_added",
            "item_done",
            "text_done",
            "turn_completed",
        ] {
            assert!(kinds.contains(&want), "missing `{want}` in {kinds:?}");
        }
        assert_eq!(kinds.last(), Some(&"turn_completed"));

        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.title.as_deref(), Some("hi"));
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].status, TurnStatus::Completed);
        assert_eq!(projection.approvals.len(), 1);
        assert!(!projection.approvals[0].decision.is_empty());
        assert!(projection.usage.total() > 0, "usage is recorded");
    }

    #[tokio::test]
    async fn trusted_mode_executes_the_tool_and_persists_output() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path(), "auto", "workspace_write");
        let events = collect(&agent, "run it").await;

        // The trusted broker still reports the decision it took, so the client
        // can show *why* a mutating call ran unattended.
        let decisions: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                AgentEvent::Approval { decision, .. } => Some(decision.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(decisions, vec!["allow"]);
        let result = tool_result(&events);
        assert!(!result.is_error);
        assert!(result.content.contains("ran echo hi"), "{}", result.content);

        // Tool output lands in the context store for later recall.
        let recalled = agent
            .context()
            .recall(&RecallQuery::new("s1", "ran echo hi"))
            .await
            .unwrap();
        assert!(
            recalled.iter().any(|f| f.content.contains("ran echo hi")),
            "tool output must be recallable"
        );

        let projection = SessionProjection::replay(
            &JsonlPersistence::new(dir.path())
                .read("s1", 0)
                .await
                .unwrap(),
        );
        assert!(projection.usage.total() > 0);
        assert_eq!(projection.turns[0].items.len(), 1);
        assert!(!projection.turns[0].items[0].is_error);
    }

    #[tokio::test]
    async fn title_is_recorded_once_per_agent() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent(dir.path(), "auto", "workspace_write");
        let _ = collect(&agent, "first prompt").await;
        let _ = collect(&agent, "second prompt").await;

        let records = JsonlPersistence::new(dir.path())
            .read("s1", 0)
            .await
            .unwrap();
        let titles = records
            .iter()
            .filter(|r| r.event.kind() == "title_set")
            .count();
        assert_eq!(titles, 1, "the deterministic title is written only once");

        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.title.as_deref(), Some("first prompt"));
        assert_eq!(projection.turns.len(), 2);
    }

    #[tokio::test]
    async fn unknown_approval_mode_and_preset_fail_closed_at_construction() {
        for (mode, preset) in [("weird", "workspace_write"), ("deny", "nonsense")] {
            let cfg = AgentConfig {
                approval_mode: mode.into(),
                permission_preset: preset.into(),
                ..AgentConfig::default()
            };
            let err = Agent::with_sandbox(
                cfg,
                Box::new(ToolThenText::default()),
                in_memory_context(),
                Arc::new(LocalSandbox),
            )
            .err()
            .expect("invalid configuration must not build");
            assert!(
                matches!(err, CoreError::Config(_)),
                "mode={mode} preset={preset} gave {err:?}"
            );
        }
    }

    #[test]
    fn policy_is_derived_from_config() {
        let cfg = AgentConfig {
            permission_preset: "read_only".into(),
            ..AgentConfig::default()
        };
        let policy = build_policy(&cfg).unwrap().expect("policy");
        assert_eq!(policy.preset(), PermissionPreset::ReadOnly);
        assert!(policy.resolve_path("a").is_ok());
        // The preset refuses mutating tools before any approval is consulted.
        assert!(policy.check_tool("fs_write").is_err());

        let bad = AgentConfig {
            permission_preset: "wat".into(),
            ..AgentConfig::default()
        };
        assert!(matches!(build_policy(&bad), Err(CoreError::Config(_))));
    }

    #[test]
    fn pipeline_and_prompt_are_resolved_once() {
        let cfg = AgentConfig {
            session: "s1".into(),
            agent_name: "tester".into(),
            instructions: "Be terse.".into(),
            ..AgentConfig::default()
        };
        let agent = Agent::with_sandbox(
            cfg,
            Box::new(ToolThenText::default()),
            in_memory_context(),
            Arc::new(LocalSandbox),
        )
        .unwrap();
        assert_eq!(agent.session(), "s1");
        assert!(agent.system_text().starts_with("You are tester."));
        assert!(agent.pipeline().policy().is_some());
        assert!(agent.handlers().is_empty());
        assert!(agent.tools().is_empty());
        assert!(agent.session_log().is_none(), "JSONL is opt-in");
        let base = agent.prompt().sections().len();
        assert!(base >= 3, "identity + instructions + policy/root");

        let with_plan = agent
            .with_prompt_sections(vec![crate::prompt::PromptSection::new("plan", 50, "1. go")]);
        assert_eq!(with_plan.prompt().sections().len(), base + 1);
        assert!(with_plan.system_text().contains("1. go"));
    }

    /// First call reports a context overflow, the retry answers.
    #[derive(Default)]
    struct OverflowThenAnswer {
        calls: AtomicUsize,
    }

    #[async_trait]
    impl ModelClient for OverflowThenAnswer {
        async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: "unused".into(),
            })
        }

        async fn complete_with_tools(
            &self,
            _req: &ModelRequest,
            _tools: &[Tool],
        ) -> Result<ModelTurn, CoreError> {
            if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(CoreError::Model(
                    "maximum context length exceeded: too many tokens".into(),
                ))
            } else {
                Ok(ModelTurn::reply("recovered"))
            }
        }
    }

    #[tokio::test]
    async fn a_context_overflow_compacts_and_retries_once() {
        let dir = tempfile::tempdir().unwrap();
        let agent = agent_with_model(dir.path(), Box::new(OverflowThenAnswer::default()));
        let events = collect(&agent, "long prompt").await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, AgentEvent::Step { phase, .. } if phase == "compact_done")),
            "the retry is preceded by a compaction"
        );
        assert!(
            matches!(events.last(), Some(AgentEvent::Done { text }) if text == "recovered"),
            "the retried call answers the turn"
        );

        let records = JsonlPersistence::new(dir.path())
            .read("s1", 0)
            .await
            .unwrap();
        assert!(records.iter().any(|r| matches!(
            &r.event,
            SessionEvent::Compacted { reason, .. } if reason == "context_overflow"
        )));
        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.compactions, 1);
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].status, TurnStatus::Completed);
    }

    #[tokio::test]
    async fn a_second_overflow_fails_the_turn() {
        struct AlwaysOverflow;

        #[async_trait]
        impl ModelClient for AlwaysOverflow {
            async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
                Err(CoreError::Model("context length exceeded".into()))
            }

            async fn complete_with_tools(
                &self,
                _req: &ModelRequest,
                _tools: &[Tool],
            ) -> Result<ModelTurn, CoreError> {
                Err(CoreError::Model("context length exceeded".into()))
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let agent = agent_with_model(dir.path(), Box::new(AlwaysOverflow));
        let mut stream = agent
            .run_event_stream("hi", &shell_tools())
            .await
            .expect("stream starts");
        let mut compactions = 0usize;
        let mut error = None;
        while let Some(ev) = stream.next().await {
            match ev {
                Ok(AgentEvent::Step { phase, .. }) if phase == "compact" => compactions += 1,
                Ok(_) => {}
                Err(e) => {
                    error = Some(e);
                    break;
                }
            }
        }
        assert_eq!(compactions, 1, "exactly one recovery attempt");
        assert!(error.is_some(), "the second overflow is terminal");

        let records = JsonlPersistence::new(dir.path())
            .read("s1", 0)
            .await
            .unwrap();
        assert_eq!(records.last().unwrap().event.kind(), "turn_failed");
    }

    #[tokio::test]
    async fn provider_reported_usage_wins_over_the_estimate() {
        struct ReportedUsage;

        #[async_trait]
        impl ModelClient for ReportedUsage {
            async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
                Ok(ModelResponse {
                    text: "unused".into(),
                })
            }

            async fn complete_with_tools(
                &self,
                _req: &ModelRequest,
                _tools: &[Tool],
            ) -> Result<ModelTurn, CoreError> {
                Ok(
                    ModelTurn::reply("done").with_usage(crate::seams::TokenUsage::new(
                        1234,
                        567,
                        "reported-model",
                    )),
                )
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let agent = agent_with_model(dir.path(), Box::new(ReportedUsage));
        let events = collect(&agent, "hi").await;
        let usage = events
            .iter()
            .find_map(|e| match e {
                AgentEvent::Usage {
                    input_tokens,
                    output_tokens,
                    model,
                } => Some((*input_tokens, *output_tokens, model.clone())),
                _ => None,
            })
            .expect("usage event");
        assert_eq!(
            usage,
            (1234, 567, "reported-model".to_string()),
            "the estimate must not override reported tokens"
        );
    }

    #[tokio::test]
    async fn an_async_title_provider_overrides_the_fallback() {
        struct CustomTitle;

        #[async_trait]
        impl crate::session::SessionTitleProvider for CustomTitle {
            async fn title(&self, _session: &str, _first_input: &str) -> Option<String> {
                Some("  Custom title  ".into())
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let agent = agent_with_model(dir.path(), Box::new(ToolThenText::default()))
            .with_title_provider(Arc::new(CustomTitle));
        let _ = collect(&agent, "hi").await;

        let records = JsonlPersistence::new(dir.path())
            .read("s1", 0)
            .await
            .unwrap();
        let sources: Vec<&str> = records
            .iter()
            .filter_map(|r| match &r.event {
                SessionEvent::TitleSet { source, .. } => Some(source.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(sources, vec!["first_prompt", "provider"]);
        assert_eq!(
            SessionProjection::replay(&records).title.as_deref(),
            Some("Custom title"),
            "the provider title is trimmed and wins"
        );
    }
}

#[cfg(test)]
mod one_shot_turn_tests {
    //! The non-agentic entry points (`run`, `run_stream`) record exactly the same
    //! turn vocabulary as `run_event_stream`. This is what makes a delegated child
    //! session — whose provider calls `run` — visible in `session_titles`.

    use super::*;
    use crate::session::{JsonlPersistence, SessionProjection, SessionTitleProvider, TurnStatus};
    use crate::{in_memory_context, ModelClient, ModelResponse};
    use async_trait::async_trait;
    use std::path::Path;

    struct EchoModel;

    #[async_trait]
    impl ModelClient for EchoModel {
        async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Ok(ModelResponse {
                text: format!("echo: {}", req.input),
            })
        }
    }

    struct BoomModel;

    #[async_trait]
    impl ModelClient for BoomModel {
        async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
            Err(CoreError::Model("model exploded".into()))
        }
    }

    /// `run` / `run_stream` never execute tools, so the sandbox is unreachable.
    struct NullSandbox;

    #[async_trait]
    impl agent_sandbox::Sandbox for NullSandbox {
        async fn spawn(
            &self,
            _spec: &agent_sandbox::ExecSpec,
        ) -> Result<agent_sandbox::SandboxHandle, agent_sandbox::SandboxError> {
            Ok(agent_sandbox::SandboxHandle { id: "null".into() })
        }

        async fn exec(
            &self,
            _handle: &agent_sandbox::SandboxHandle,
            _cmd: &[String],
        ) -> Result<agent_sandbox::ExecOutput, agent_sandbox::SandboxError> {
            Err(agent_sandbox::SandboxError::Exec(
                "no sandbox in this test".into(),
            ))
        }

        async fn destroy(
            &self,
            _handle: agent_sandbox::SandboxHandle,
        ) -> Result<(), agent_sandbox::SandboxError> {
            Ok(())
        }
    }

    struct FixedTitle(&'static str);

    #[async_trait]
    impl SessionTitleProvider for FixedTitle {
        async fn title(&self, _session: &str, _first_input: &str) -> Option<String> {
            Some(self.0.to_string())
        }
    }

    fn one_shot(dir: &Path, model: Box<dyn ModelClient>, session: &str) -> Agent {
        let root = dir.to_string_lossy().to_string();
        let cfg = AgentConfig {
            session: session.into(),
            agent_name: "tester".into(),
            workspace_root: root.clone(),
            session_log_dir: root,
            ..AgentConfig::default()
        };
        Agent::with_sandbox(cfg, model, in_memory_context(), Arc::new(NullSandbox))
            .expect("agent builds")
    }

    async fn records(dir: &Path, session: &str) -> Vec<crate::session::SessionRecord> {
        JsonlPersistence::new(dir)
            .read(session, 0)
            .await
            .expect("session log written")
    }

    fn kinds(records: &[crate::session::SessionRecord]) -> Vec<&str> {
        records.iter().map(|r| r.event.kind()).collect()
    }

    #[tokio::test]
    async fn run_records_the_turn_of_a_child_session() {
        let dir = tempfile::tempdir().unwrap();
        // A delegated child runs with its own namespaced session id.
        let child = crate::seams::child_session("parent-1", "abc");
        let agent = one_shot(dir.path(), Box::new(EchoModel), &child);

        assert_eq!(
            agent.run("summarise the repo").await.expect("run"),
            "echo: summarise the repo"
        );

        let records = records(dir.path(), &child).await;
        for want in ["turn_started", "title_set", "text_done", "turn_completed"] {
            let kinds = kinds(&records);
            assert!(kinds.contains(&want), "missing `{want}` in {kinds:?}");
        }
        assert_eq!(kinds(&records).last(), Some(&"turn_completed"));

        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.title.as_deref(), Some("summarise the repo"));
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].status, TurnStatus::Completed);
        // The child id walks back to the session that owns it.
        assert_eq!(crate::seams::parent_session(&child), Some("parent-1"));
    }

    #[tokio::test]
    async fn run_honours_the_title_provider_override() {
        let dir = tempfile::tempdir().unwrap();
        let agent = one_shot(dir.path(), Box::new(EchoModel), "s-provider")
            .with_title_provider(Arc::new(FixedTitle("Release triage")));

        agent.run("hello there").await.expect("run");

        let records = records(dir.path(), "s-provider").await;
        let sources: Vec<&str> = records
            .iter()
            .filter_map(|r| match &r.event {
                SessionEvent::TitleSet { source, .. } => Some(source.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(sources, vec!["first_prompt", "provider"]);
        assert_eq!(
            SessionProjection::replay(&records).title.as_deref(),
            Some("Release triage")
        );
    }

    #[tokio::test]
    async fn a_failed_run_still_titles_the_session_and_closes_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let agent = one_shot(dir.path(), Box::new(BoomModel), "s-boom");

        assert!(agent.run("hi").await.is_err());

        let records = records(dir.path(), "s-boom").await;
        assert_eq!(kinds(&records).last(), Some(&"turn_failed"));
        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.title.as_deref(), Some("hi"));
        assert_eq!(projection.turns.len(), 1);
        assert_eq!(projection.turns[0].status, TurnStatus::Failed);
    }

    #[tokio::test]
    async fn run_stream_records_the_turn() {
        let dir = tempfile::tempdir().unwrap();
        let agent = one_shot(dir.path(), Box::new(EchoModel), "s-stream");

        let mut stream = agent.run_stream("stream me").await.expect("stream starts");
        let mut out = String::new();
        while let Some(item) = stream.next().await {
            out.push_str(&item.expect("token"));
        }
        assert_eq!(out, "echo: stream me");

        let records = records(dir.path(), "s-stream").await;
        assert_eq!(kinds(&records).last(), Some(&"turn_completed"));
        let projection = SessionProjection::replay(&records);
        assert_eq!(projection.title.as_deref(), Some("stream me"));
        assert_eq!(projection.turns[0].status, TurnStatus::Completed);
    }
}
