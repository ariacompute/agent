//! Parked human-in-the-loop decisions for one A2A task.
//!
//! A2A can expose `input-required` and wait for `tasks/resume`, but the agent
//! seams (`ApprovalBroker`, `UserQuestions`) are plain `await`s. [`PausingHitl`]
//! is the adapter: it moves the task to `input-required`, publishes a final
//! stream frame, and parks the seam call on a one-shot channel until
//! `tasks/resume` answers it.
//!
//! The same object serves the dsh harness through [`RoutingBridge`]: dsh's
//! approval/question requests are plain JSON-RPC calls on a per-tenant handler,
//! so the bridge looks the session up in [`A2aRuntime`] and delegates to the
//! parked task. Native runs inject this object directly as the two seams.
//!
//! Ordering rules that keep this race-free:
//!
//! * the pending slot is filled **before** the state moves to `input-required`,
//!   and `resume` refuses unless the stored state is `input-required`, so an
//!   answer can never be consumed by a task that has not parked yet;
//! * `resume` owns the `input-required → working` transition; the parked side
//!   only waits for the answer;
//! * a cancelled or failed task leaves the slot empty and the sender dropped, so
//!   the parked call returns a denial instead of hanging.

use std::sync::Arc;

use agent_core::seams::{
    ApprovalBroker, ApprovalDecision, ApprovalRequest, PermissionPreset, SeamError, SeamResult,
    UserQuestions,
};
use aria_agent_a2a::types::{StreamFrame, TaskStatus, TaskStatusUpdateEvent};
use aria_agent_a2a::TaskState;
use aria_agent_dsh::rpc::{
    ServerRequestHandler, CODE_METHOD_NOT_FOUND, METHOD_APPROVAL_REQUEST, METHOD_USER_QUESTION,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::sync::{broadcast, oneshot, Mutex};

use crate::auth::Principal;
use crate::util::now_ms;

use super::push::PushNotifier;
use super::runtime::A2aRuntime;
use super::store::{A2aTaskRow, A2aTaskStore};

/// JSON-RPC "invalid params".
pub const CODE_INVALID_PARAMS: i64 = -32602;

/// What the parked task is waiting for.
#[derive(Debug, Clone, PartialEq)]
enum PendingKind {
    Approval {
        tool: String,
    },
    Question {
        question: String,
        options: Vec<String>,
    },
}

impl PendingKind {
    /// The `input-required` status note the caller sees.
    fn note(&self) -> String {
        match self {
            PendingKind::Approval { tool } => format!("approval required for `{tool}`"),
            PendingKind::Question { question, options } if options.is_empty() => question.clone(),
            PendingKind::Question { question, options } => {
                format!("{question} (options: {})", options.join(", "))
            }
        }
    }
}

/// The answer handed back to the parked call.
#[derive(Debug, Clone, PartialEq)]
enum Answer {
    Decision(ApprovalDecision),
    Text(String),
}

struct Pending {
    kind: PendingKind,
    answer: oneshot::Sender<Answer>,
}

/// An answer supplied through `tasks/resume`.
#[derive(Debug, Clone, PartialEq)]
pub enum ResumeInput {
    /// `true` = allow, `false` = deny.
    Decision(bool),
    /// Free text, for a question.
    Text(String),
}

/// Why `resume` could not continue a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeError {
    /// Nothing is parked (already answered, cancelled, or never asked).
    NotParked,
    /// The answer does not fit the pending request (text for an approval, …).
    Mismatch,
}

/// Everything one parked task needs. Grouped so the constructor stays readable.
pub struct HitlContext {
    pub principal: Principal,
    pub task_id: String,
    /// The session (and container) the task runs in.
    pub context_id: String,
    pub preset: PermissionPreset,
    pub store: Arc<dyn A2aTaskStore>,
    pub push: Arc<dyn PushNotifier>,
    pub frames: broadcast::Sender<StreamFrame>,
}

/// One task's parked-decision channel.
pub struct PausingHitl {
    principal: Principal,
    task_id: String,
    context_id: String,
    preset: PermissionPreset,
    store: Arc<dyn A2aTaskStore>,
    push: Arc<dyn PushNotifier>,
    frames: broadcast::Sender<StreamFrame>,
    pending: Mutex<Option<Pending>>,
}

impl PausingHitl {
    pub fn new(ctx: HitlContext) -> Self {
        Self {
            principal: ctx.principal,
            task_id: ctx.task_id,
            context_id: ctx.context_id,
            preset: ctx.preset,
            store: ctx.store,
            push: ctx.push,
            frames: ctx.frames,
            pending: Mutex::new(None),
        }
    }

    pub fn task_id(&self) -> &str {
        &self.task_id
    }

    fn emit(&self, frame: StreamFrame) {
        let _ = self.frames.send(frame);
    }

    /// Move to `input-required` and publish the final status frame.
    async fn park(&self, note: String) -> bool {
        let status = TaskStatus::with_message(TaskState::InputRequired, note);
        let transitioned = self
            .store
            .transition(
                &self.principal,
                &self.task_id,
                TaskState::InputRequired,
                Some(json!(status)),
                now_ms(),
            )
            .await;
        match transitioned {
            Ok(Some(row)) => {
                self.emit(StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
                    self.task_id.clone(),
                    self.context_id.clone(),
                    status,
                )));
                self.notify(&row, "input-required").await;
                true
            }
            // Already terminal (cancelled) or an illegal move: fail closed.
            _ => false,
        }
    }

    /// Fill the slot (one pending request at a time), then park.
    async fn begin(&self, pending: Pending) -> bool {
        let note = pending.kind.note();
        {
            let mut slot = self.pending.lock().await;
            if slot.is_some() {
                return false;
            }
            *slot = Some(pending);
        }
        if self.park(note).await {
            return true;
        }
        self.pending.lock().await.take();
        false
    }

    /// Release a parked decision without answering it.
    ///
    /// `tasks/cancel` uses this: aborting the run task would drop the parked
    /// future, but an explicit release stops the seam call first, so the run
    /// unwinds through its own code instead of being torn down mid-`.await`.
    pub async fn abandon(&self) {
        let pending = self.pending.lock().await.take();
        drop(pending);
    }

    /// Answer a parked task. `resume` owns the transition back to `working`.
    ///
    /// A text answer is accepted for an approval too, but only when it is
    /// actually a decision (`allow` / `deny` / `yes` / `no` …): an unparsed
    /// answer is a [`ResumeError::Mismatch`], never an implicit denial, and the
    /// task stays `input-required` so the caller can try again. A decision
    /// cannot answer a free-text question either — same error, same state.
    pub async fn resume(&self, input: ResumeInput) -> Result<TaskState, ResumeError> {
        let mut slot = self.pending.lock().await;
        let Some(pending) = slot.take() else {
            return Err(ResumeError::NotParked);
        };
        // Only a task the store says is parked may be resumed. This rejects a
        // resume that raced the park (the slot is filled first).
        let row = match self.store.get(&self.principal, &self.task_id).await {
            Ok(row) => row,
            Err(_) => {
                *slot = Some(pending);
                return Err(ResumeError::NotParked);
            }
        };
        if row.state != TaskState::InputRequired {
            *slot = Some(pending);
            return Err(ResumeError::NotParked);
        }
        let answer = match (&pending.kind, &input) {
            (PendingKind::Approval { .. }, ResumeInput::Decision(allow)) => {
                Answer::Decision(if *allow {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny {
                        reason: "denied by the caller".into(),
                    }
                })
            }
            (PendingKind::Approval { .. }, ResumeInput::Text(text)) => {
                let Some(allow) = parse_decision(text) else {
                    *slot = Some(pending);
                    return Err(ResumeError::Mismatch);
                };
                Answer::Decision(if allow {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Deny {
                        reason: "denied by the caller".into(),
                    }
                })
            }
            (PendingKind::Question { .. }, ResumeInput::Text(text)) => Answer::Text(text.clone()),
            (PendingKind::Question { .. }, ResumeInput::Decision(_)) => {
                *slot = Some(pending);
                return Err(ResumeError::Mismatch);
            }
        };
        let status = TaskStatus::now(TaskState::Working);
        let moved = self
            .store
            .transition(
                &self.principal,
                &self.task_id,
                TaskState::Working,
                Some(json!(status)),
                now_ms(),
            )
            .await;
        match moved {
            Ok(Some(_)) => {}
            _ => {
                *slot = Some(pending);
                return Err(ResumeError::NotParked);
            }
        }
        let _ = pending.answer.send(answer);
        drop(slot);
        self.emit(StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
            self.task_id.clone(),
            self.context_id.clone(),
            status,
        )));
        Ok(TaskState::Working)
    }

    async fn notify(&self, row: &A2aTaskRow, state: &str) {
        let Some(config) = &row.push else {
            return;
        };
        let body = json!({
            "taskId": row.task_id,
            "contextId": row.context_id,
            "state": state,
            "status": row.status,
            "config": config,
        });
        if let Err(e) = self.push.notify(config, &body).await {
            tracing::warn!(task_id = %row.task_id, error = %e, "a2a push notification failed");
        }
    }

    async fn suspend_approval(&self, req: &ApprovalRequest) -> ApprovalDecision {
        let (tx, rx) = oneshot::channel();
        let pending = Pending {
            kind: PendingKind::Approval {
                tool: req.tool.clone(),
            },
            answer: tx,
        };
        if !self.begin(pending).await {
            return ApprovalDecision::Deny {
                reason: "the task is no longer live".into(),
            };
        }
        match rx.await {
            Ok(Answer::Decision(decision)) => decision,
            // `resume` never hands text to an approval, so anything else means
            // the deciding side went away without answering.
            _ => ApprovalDecision::Deny {
                reason: "the approval channel closed".into(),
            },
        }
    }
    async fn suspend_question(&self, question: &str, options: &[String]) -> SeamResult<String> {
        let (tx, rx) = oneshot::channel();
        let pending = Pending {
            kind: PendingKind::Question {
                question: question.to_string(),
                options: options.to_vec(),
            },
            answer: tx,
        };
        if !self.begin(pending).await {
            return Err(SeamError::Unavailable("the task is no longer live".into()));
        }
        match rx.await {
            Ok(Answer::Text(text)) => Ok(text),
            // `resume` refuses to send a decision to a question
            // (`ResumeError::Mismatch`), so everything else means the deciding
            // side went away without answering.
            Ok(_) | Err(_) => Err(SeamError::Unavailable("the question channel closed".into())),
        }
    }
}

#[async_trait]
impl ApprovalBroker for PausingHitl {
    async fn decide(&self, req: &ApprovalRequest) -> ApprovalDecision {
        self.suspend_approval(req).await
    }
}

#[async_trait]
impl UserQuestions for PausingHitl {
    async fn ask(&self, question: &str, options: &[String]) -> SeamResult<String> {
        self.suspend_question(question, options).await
    }
}

#[async_trait]
impl ServerRequestHandler for PausingHitl {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            METHOD_APPROVAL_REQUEST => {
                let tool = params.get("tool").and_then(Value::as_str).ok_or_else(|| {
                    (
                        CODE_INVALID_PARAMS,
                        "approval request without `tool`".to_string(),
                    )
                })?;
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .unwrap_or(&self.context_id)
                    .to_string();
                let request =
                    ApprovalRequest::new(tool, session, arguments).with_preset(self.preset);
                let decision = self.decide(&request).await;
                Ok(if decision.is_allowed() {
                    json!({"decision": "allow"})
                } else {
                    json!({"decision": "deny", "reason": decision.reason()})
                })
            }
            METHOD_USER_QUESTION => {
                let question = params
                    .get("question")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        (
                            CODE_INVALID_PARAMS,
                            "question request without `question`".to_string(),
                        )
                    })?;
                let options: Vec<String> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                match self.ask(question, &options).await {
                    Ok(answer) => Ok(json!({"answer": answer})),
                    Err(e) => Err((CODE_METHOD_NOT_FOUND, e.to_string())),
                }
            }
            other => Err((
                CODE_METHOD_NOT_FOUND,
                format!("unsupported dsh request: {other}"),
            )),
        }
    }
}

/// dsh requests arrive on the per-tenant handler; this bridge sends the ones
/// that name a session with a parked A2A task to that task, and everything else
/// to the deployment's own bridge (which keeps its fail-closed defaults).
pub struct RoutingBridge {
    runtime: Arc<A2aRuntime>,
    fallback: Arc<dyn ServerRequestHandler>,
}

impl RoutingBridge {
    pub fn new(runtime: Arc<A2aRuntime>, fallback: Arc<dyn ServerRequestHandler>) -> Self {
        Self { runtime, fallback }
    }
}

#[async_trait]
impl ServerRequestHandler for RoutingBridge {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        if let Some(session) = params.get("sessionId").and_then(Value::as_str) {
            if let Some(hitl) = self.runtime.hitl_for(session).await {
                return hitl.handle(method, params).await;
            }
        }
        self.fallback.handle(method, params).await
    }
}

/// Parse a decision spelled as text. `None` means "not a decision".
pub fn parse_decision(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "approve" | "approved" | "allow" | "allowed" | "yes" | "true" | "ok" => Some(true),
        "deny" | "denied" | "reject" | "rejected" | "no" | "false" => Some(false),
        _ => None,
    }
}
