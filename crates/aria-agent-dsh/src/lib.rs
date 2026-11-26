//! DeepSeek Harness (dsh) as the agent turn runtime.
//!
//! dsh's `sdk` profile is a complete agent runtime: it owns turn/step
//! orchestration, the model call, tool execution and its own sandbox. This crate
//! drives it as a **child process over newline-delimited JSON-RPC** and plugs the
//! result into the [`AgentLoop`] seam, so:
//!
//! * the cloud's streaming contract is untouched — we only emit `AgentEvent`s,
//!   which `aria-agent-cloud::event_envelope` already translates (ADR-0009);
//! * approvals and user questions keep going through the installed seams
//!   ([`approval::SeamApprovalBridge`]), with fail-closed defaults;
//! * `review` / titles / turns keep the same `SessionEvent` vocabulary, so
//!   session query and the title projection stay backend-agnostic.
//!
//! Wiring (see `docs/adr/0019-dsh-runtime.md`):
//!
//! ```no_run
//! # use std::sync::Arc;
//! # use std::time::Duration;
//! # use agent_core::seams::{AutoAllow, NoQuestions, PermissionPreset};
//! # use aria_agent_dsh::{DshAgentLoop, DshConfig, DshPool, SeamApprovalBridge};
//! let bridge = Arc::new(SeamApprovalBridge::new(
//!     Arc::new(AutoAllow),
//!     Arc::new(NoQuestions),
//!     "",
//!     PermissionPreset::WorkspaceWrite,
//! ));
//! let cfg = DshConfig::for_session(std::path::Path::new("/srv/agents"), "s1", "deepseek-v4");
//! let pool = Arc::new(DshPool::new(cfg, 8, Duration::from_secs(900), bridge));
//! let harness = Arc::new(DshAgentLoop::new(pool));
//! // `Agent::with_components(cfg, model, context, sandbox, handlers, harness)`
//! # let _ = harness;
//! ```

pub mod approval;
pub mod event_map;
pub mod rpc;
pub mod runtime;
pub mod thinking_proxy;
pub mod transport;

pub use approval::SeamApprovalBridge;
pub use event_map::Mapped;
pub use rpc::{
    RpcClient, ServerRequestHandler, UnsupportedRequests, CODE_METHOD_NOT_FOUND,
    DEFAULT_NOTIFICATION_CAPACITY, METHOD_APPROVAL_REQUEST, METHOD_INITIALIZE,
    METHOD_SESSION_PROMPT, METHOD_USER_QUESTION,
};
pub use runtime::{
    no_bridge, session_dir_name, DshConfig, DshConnector, DshPool, DshRuntime, HarnessRuntime,
    RuntimeConnector, PERMISSION_MODES, SDK_PROFILE,
};
pub use thinking_proxy::{
    parse_filter_mode, strip_redacted_from_messages_json, strip_redacted_from_response_json,
    SseRedactedFilter, ThinkingFilterProxy, FILTER_DISABLE, FILTER_ENABLE,
};
pub use transport::{RpcTransport, StdioTransport, DEFAULT_MAX_FRAME_BYTES};

use async_trait::async_trait;
use futures::stream::BoxStream;
use serde_json::json;
use std::sync::Arc;

use agent_core::agent_loop::{fail_turn, record_turn_open, recorder_for};
use agent_core::context::{ContextFragment, FragmentKind, RelationKind};
use agent_core::seams::{AgentLoop, LoopContext};
use agent_core::session::SessionEvent;
use agent_core::{graph_relate_store, maybe_compact_store, AgentEvent, CoreError, ToolResult};

/// `AgentLoop` implementation that delegates the whole turn to dsh.
pub struct DshAgentLoop {
    pool: Arc<DshPool>,
}

impl DshAgentLoop {
    pub fn new(pool: Arc<DshPool>) -> Self {
        Self { pool }
    }

    /// The pool (diagnostics / warm-up).
    pub fn pool(&self) -> &Arc<DshPool> {
        &self.pool
    }
}

#[async_trait]
impl AgentLoop for DshAgentLoop {
    async fn run(
        &self,
        ctx: LoopContext,
        input: &str,
    ) -> Result<BoxStream<'static, Result<AgentEvent, CoreError>>, CoreError> {
        let session = ctx.config.session.clone();
        let mut recorder = recorder_for(&session, ctx.session_log.as_ref());
        let turn_id = uuid::Uuid::new_v4().to_string();
        // Titles are recorded (and flushed) before the harness is touched: a
        // runtime that hangs or dies must not lose the session title (ADR-0015).
        record_turn_open(
            &mut recorder,
            &ctx.title_pending,
            &ctx.title_provider,
            &session,
            &turn_id,
            input,
        )
        .await;
        let _ = recorder.flush().await;

        // Persist the user turn before prompting: recall for the *next* turn must
        // see it even if the harness dies mid-turn.
        let user = ContextFragment::new(&session, FragmentKind::Message, input);
        let user_id = user.id.clone();
        ctx.context.memorize(user).await?;

        let recorder = Arc::new(tokio::sync::Mutex::new(recorder));
        let runtime = match self.pool.acquire(&session).await {
            Ok(runtime) => runtime,
            Err(e) => {
                fail_turn(&recorder, &turn_id, &e).await;
                return Err(e);
            }
        };
        let rx = match runtime.prompt(&session, input).await {
            Ok(rx) => rx,
            Err(e) => {
                fail_turn(&recorder, &turn_id, &e).await;
                return Err(e);
            }
        };

        let mut rx = rx;
        let stream = async_stream::stream! {
            let recorder = recorder;
            let mut text = String::new();
            let mut terminal = false;
            let mut last_failure: Option<String> = None;
            while let Some(frame) = rx.recv().await {
                if let Some(detail) = event_map::failure_detail(&frame) {
                    last_failure = Some(detail);
                }
                for mapped in event_map::map_notification(&frame) {
                    match mapped {
                        Mapped::Step { phase, label } => {
                            yield Ok(AgentEvent::Step { phase: phase.to_string(), label: label.clone() });
                            recorder.lock().await.push(SessionEvent::Step {
                                phase: phase.to_string(),
                                label,
                            });
                        }
                        Mapped::Usage { input_tokens, output_tokens, model } => {
                            yield Ok(AgentEvent::Usage {
                                input_tokens,
                                output_tokens,
                                model: model.clone(),
                            });
                            recorder.lock().await.push(SessionEvent::Usage {
                                input_tokens,
                                output_tokens,
                                model,
                            });
                        }
                        Mapped::Token { text: delta } => {
                            text = delta.clone();
                            yield Ok(AgentEvent::Token { text: delta.clone() });
                            recorder.lock().await.push(SessionEvent::TextDone { text: delta });
                        }
                        Mapped::ToolCall { id, name, arguments, content, is_error } => {
                            {
                                let mut guard = recorder.lock().await;
                                guard.push(SessionEvent::ItemAdded {
                                    item_id: id.clone(),
                                    kind: "tool_call".into(),
                                    payload: json!({"name": name, "arguments": arguments}),
                                });
                                guard.push(SessionEvent::ItemDone {
                                    item_id: id.clone(),
                                    is_error,
                                    result: json!({"content": content, "spill": content.len()}),
                                    // dsh does not report per-call timing; the
                                    // report degrades to step/retry dimensions
                                    // rather than inventing a number.
                                    latency_ms: None,
                                });
                            }
                            yield Ok(AgentEvent::ToolCall {
                                id: id.clone(),
                                name,
                                arguments,
                                result: ToolResult {
                                    call_id: id,
                                    content,
                                    is_error,
                                },
                            });
                        }
                        Mapped::TurnEnd { reason } => {
                            terminal = true;
                            if reason == "error" {
                                let message = with_failure_detail(
                                    "dsh finished the turn with reason `error`",
                                    last_failure.take(),
                                );
                                let mut guard = recorder.lock().await;
                                guard.push(SessionEvent::TurnFailed {
                                    turn_id: turn_id.clone(),
                                    message: message.clone(),
                                });
                                let _ = guard.flush().await;
                                yield Err(CoreError::Model(message));
                            } else {
                                memorize_assistant(&ctx, &user_id, &text).await;
                                let mut guard = recorder.lock().await;
                                guard.push(SessionEvent::TurnCompleted {
                                    turn_id: turn_id.clone(),
                                    output: text.clone(),
                                });
                                let _ = guard.flush().await;
                                yield Ok(AgentEvent::Done { text: text.clone() });
                            }
                        }
                        Mapped::TurnFailed { message } => {
                            terminal = true;
                            let message = with_failure_detail(message, last_failure.take());
                            let mut guard = recorder.lock().await;
                            guard.push(SessionEvent::TurnFailed {
                                turn_id: turn_id.clone(),
                                message: message.clone(),
                            });
                            let _ = guard.flush().await;
                            yield Err(CoreError::Model(message));
                        }
                        Mapped::Protocol { message } => {
                            terminal = true;
                            let message = format!("dsh protocol error: {message}");
                            let mut guard = recorder.lock().await;
                            guard.push(SessionEvent::TurnFailed {
                                turn_id: turn_id.clone(),
                                message: message.clone(),
                            });
                            let _ = guard.flush().await;
                            yield Err(CoreError::Model(message));
                        }
                        Mapped::Ignore => {}
                    }
                }
                if terminal {
                    break;
                }
            }
            if !terminal {
                // The runtime vanished without a terminal frame: fail the turn
                // loudly instead of reporting a silent empty answer.
                let message = "dsh runtime closed the stream without `turn/end`".to_string();
                let mut guard = recorder.lock().await;
                guard.push(SessionEvent::TurnFailed {
                    turn_id: turn_id.clone(),
                    message: message.clone(),
                });
                let _ = guard.flush().await;
                yield Err(CoreError::Model(message));
            }
        };
        Ok(Box::pin(stream))
    }
}

/// Append the last structured dsh failure detail to a terminal error message.
/// Details already present in the base message are not repeated.
fn with_failure_detail(base: impl Into<String>, detail: Option<String>) -> String {
    let base = base.into();
    match detail {
        Some(detail) if !detail.is_empty() && !base.contains(&detail) => {
            format!("{base}: {detail}")
        }
        _ => base,
    }
}

/// Remember the assistant text and keep the graph/compaction invariants the
/// native loop upholds (ADR-0015), so switching runtimes does not change memory.
async fn memorize_assistant(ctx: &LoopContext, user_id: &str, text: &str) {
    if text.is_empty() {
        return;
    }
    let asst = ContextFragment::new(ctx.session(), FragmentKind::Message, text.to_string());
    let asst_id = asst.id.clone();
    let _ = ctx.context.memorize(asst).await;
    if ctx.config.memory_graph {
        graph_relate_store(
            &ctx.context,
            &ctx.config.session,
            user_id,
            &asst_id,
            RelationKind::Temporal,
        )
        .await;
        maybe_compact_store(
            &ctx.context,
            &ctx.config.session,
            ctx.config.memory_compact_threshold,
        )
        .await;
    }
}
