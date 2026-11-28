//! Executing one A2A message: session mirror, run task, frame projection.
//!
//! A `message/send` or `message/stream` call always ends up here. The HTTP
//! handler gets a [`Started`] (task id, submitted task, frame receiver) and the
//! work itself runs in a spawned task, so:
//!
//! * `message/send` can wait for the first `final` frame without owning the run;
//! * `tasks/cancel` can abort the run and destroy its container;
//! * a client that disconnects changes nothing about the task.
//!
//! What is **not** written to the event log: token deltas. They are broadcast to
//! live subscribers only. The log keeps the coarse frames (task, status,
//! artifact, message), which is what `GET /a2a/stream` replays.

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use agent_core::seams::PermissionPreset;
use agent_core::{AgentConfig, AgentEvent};
use aria_agent_a2a::types::{
    Artifact, Message, PushNotificationConfig, StreamFrame, Task, TaskArtifactUpdateEvent,
    TaskStatus, TaskStatusUpdateEvent,
};
use aria_agent_a2a::TaskState;
use futures::StreamExt;
use serde_json::{json, Value};
use tokio::sync::broadcast;

use crate::auth::Principal;
use crate::store::{ItemRow, SessionRow, TurnRow};
use crate::util::{new_id, now_ms};
use crate::{AppError, AppState};

use super::hitl::{HitlContext, PausingHitl, ResumeError, ResumeInput};
use super::push::PushNotifier;
use super::runtime::{A2aRuntime, ContextBusy};
use super::store::{A2aTaskRow, A2aTaskStore};

/// The id of the final artifact, and the one token chunks append to.
pub const FINAL_ARTIFACT: &str = "final";

/// A task that was accepted and spawned.
pub struct Started {
    pub task_id: String,
    pub task: Task,
    pub frames: broadcast::Receiver<StreamFrame>,
}

/// Why a message could not be accepted.
#[derive(Debug)]
pub enum StartError {
    /// No text part at all.
    Empty,
    /// Another live task already owns the context (`-32005`).
    ContextBusy,
    Store(AppError),
}

/// `message/send` / `message/stream` input after agent resolution.
pub struct StartRequest {
    pub principal: Principal,
    pub agent: crate::types::Agent,
    pub message: Message,
    pub push: Option<PushNotificationConfig>,
}

/// Accept a message: mirror the session, persist the task and spawn the run.
pub async fn start(state: &AppState, req: StartRequest) -> Result<Started, StartError> {
    let input = req.message.text();
    if input.trim().is_empty() {
        return Err(StartError::Empty);
    }
    let principal = req.principal.clone();
    let (context_id, created) = match req.message.context() {
        Some(existing) => match state.sessions.get_session(&principal, existing).await {
            Ok(row) => (row.id, false),
            Err(AppError::NotFound) => (new_id("sess"), true),
            Err(e) => return Err(StartError::Store(e)),
        },
        None => (new_id("sess"), true),
    };
    let task_id = new_id("task");
    let turn_id = new_id("turn");
    let agent_id = req.agent.id.clone();
    let agent_name = req
        .agent
        .name
        .clone()
        .unwrap_or_else(|| req.agent.id.clone());
    let instructions = req.agent.instructions.clone().unwrap_or_default();

    // Reserve the context before writing anything: a second live task in the
    // same session is refused without leaving a submitted row behind.
    let channel = state
        .a2a
        .runtime
        .open(&task_id, &context_id)
        .await
        .map_err(|ContextBusy| StartError::ContextBusy)?;
    if let Err(e) = mirror_session(
        state,
        &principal,
        &context_id,
        &turn_id,
        &agent_id,
        created,
        &input,
    )
    .await
    {
        state.a2a.runtime.finish(&task_id).await;
        return Err(StartError::Store(e));
    }

    let status = TaskStatus::now(TaskState::Submitted);
    let row = A2aTaskRow::submitted(
        &task_id,
        &principal.id,
        &agent_id,
        &context_id,
        &turn_id,
        json!(status),
        now_ms(),
    );
    if let Err(e) = state.a2a.tasks.create(&principal, &row).await {
        state.a2a.runtime.finish(&task_id).await;
        return Err(StartError::Store(e));
    }
    let mut inbound = req.message.clone();
    inbound.message_id = if inbound.message_id.is_empty() {
        new_id("msg")
    } else {
        inbound.message_id.clone()
    };
    inbound.context_id = Some(context_id.clone());
    inbound.task_id = Some(task_id.clone());
    let task = Task {
        kind: "task".into(),
        id: task_id.clone(),
        context_id: context_id.clone(),
        session_id: None,
        status,
        artifacts: Vec::new(),
        history: Some(vec![inbound.clone()]),
        metadata: None,
    };
    let _ = state
        .a2a
        .tasks
        .append_event(&principal, &task_id, "task", &json!(task))
        .await;
    let _ = state
        .a2a
        .tasks
        .append_event(&principal, &task_id, "message", &json!(inbound))
        .await;
    if let Some(push) = &req.push {
        if let Err(e) = state.a2a.tasks.set_push(&principal, &task_id, push).await {
            state.a2a.runtime.finish(&task_id).await;
            return Err(StartError::Store(e));
        }
    }

    let mut cfg: AgentConfig = crate::app::agent_config(&agent_name, &instructions, &context_id);
    cfg.agent_name = agent_name;
    cfg.session = context_id.clone();
    if let Some(model) = &req.agent.model {
        cfg.model = model.clone();
    }
    let preset = PermissionPreset::parse(&cfg.permission_preset).unwrap_or_default();
    let hitl = Arc::new(PausingHitl::new(HitlContext {
        principal: principal.clone(),
        task_id: task_id.clone(),
        context_id: context_id.clone(),
        preset,
        store: state.a2a.tasks.clone(),
        push: state.a2a.push.clone(),
        frames: channel.frames.clone(),
    }));
    state
        .a2a
        .runtime
        .attach_hitl(&context_id, hitl.clone())
        .await;

    let run = RunTask {
        state: state.clone(),
        principal,
        cfg,
        input,
        hitl,
        task_id: task_id.clone(),
        context_id,
        turn_id,
        frames: channel.frames,
        store: state.a2a.tasks.clone(),
        push: state.a2a.push.clone(),
        runtime: state.a2a.runtime.clone(),
        seq: AtomicI32::new(0),
    };
    let join = tokio::spawn(async move { run.run().await });
    state.a2a.runtime.set_join(&task_id, join).await;
    Ok(Started {
        task_id,
        task,
        frames: channel.receiver,
    })
}

/// Create the session (only when new) and open the turn, so the existing console
/// shows the A2A conversation like any other.
async fn mirror_session(
    state: &AppState,
    principal: &Principal,
    context_id: &str,
    turn_id: &str,
    agent_id: &str,
    created: bool,
    input: &str,
) -> Result<(), AppError> {
    if created {
        state
            .sessions
            .insert_session(
                principal,
                &SessionRow {
                    id: context_id.to_string(),
                    principal_id: principal.id.clone(),
                    agent_id: agent_id.to_string(),
                    instructions: None,
                    status: "active".into(),
                    created_at: now_ms(),
                },
            )
            .await?;
    }
    state
        .sessions
        .insert_turn(&TurnRow {
            id: turn_id.to_string(),
            principal_id: principal.id.clone(),
            session_id: context_id.to_string(),
            agent_id: agent_id.to_string(),
            status: "in_progress".into(),
            output: None,
            created_at: now_ms(),
            completed_at: None,
        })
        .await?;
    state
        .sessions
        .insert_item(&ItemRow {
            id: new_id("item"),
            principal_id: principal.id.clone(),
            session_id: context_id.to_string(),
            turn_id: turn_id.to_string(),
            kind: "message".into(),
            role: Some("user".into()),
            status: "completed".into(),
            content: Value::String(input.to_string()),
            seq: 0,
            created_at: now_ms(),
        })
        .await?;
    Ok(())
}

/// Everything one run needs; grouped because the spawned future owns all of it.
struct RunTask {
    state: AppState,
    principal: Principal,
    cfg: AgentConfig,
    input: String,
    hitl: Arc<PausingHitl>,
    task_id: String,
    context_id: String,
    turn_id: String,
    frames: broadcast::Sender<StreamFrame>,
    store: Arc<dyn A2aTaskStore>,
    push: Arc<dyn PushNotifier>,
    runtime: Arc<A2aRuntime>,
    /// Monotonic `session_items.seq` inside this run.
    seq: AtomicI32,
}

impl RunTask {
    async fn run(self) {
        let agent = match crate::app::build_agent_for_a2a(
            &self.state,
            self.cfg.clone(),
            &self.principal,
            self.hitl.clone(),
            self.hitl.clone(),
        )
        .await
        {
            Ok(agent) => agent,
            Err(e) => {
                self.fail(&format!("cannot build the agent: {e:?}")).await;
                return;
            }
        };
        let status = TaskStatus::now(TaskState::Working);
        if self
            .transition(TaskState::Working, Some(json!(status)))
            .await
            .is_none()
        {
            self.cleanup().await;
            return;
        }
        self.emit(StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
            self.task_id.clone(),
            self.context_id.clone(),
            status,
        )));

        let tools = agent.tools().to_vec();
        let mut stream = match agent.run_event_stream(&self.input, &tools).await {
            Ok(stream) => stream,
            Err(e) => {
                self.fail(&e.to_string()).await;
                return;
            }
        };
        let mut collected = String::new();
        let mut streamed = false;
        while let Some(item) = stream.next().await {
            match item {
                Ok(AgentEvent::Token { text }) => {
                    collected.push_str(&text);
                    self.emit(StreamFrame::ArtifactUpdate(TaskArtifactUpdateEvent::chunk(
                        self.task_id.clone(),
                        self.context_id.clone(),
                        token_artifact(&text),
                        true,
                        false,
                    )));
                    streamed = true;
                }
                Ok(AgentEvent::Done { text }) => {
                    if !text.is_empty() {
                        collected = text;
                    }
                }
                Ok(AgentEvent::Step { phase, label }) => {
                    let mut update = TaskStatusUpdateEvent::progress(
                        self.task_id.clone(),
                        self.context_id.clone(),
                        TaskStatus::now(TaskState::Working),
                    );
                    update.metadata = Some(json!({
                        "phase": phase,
                        "label": label,
                        "event": "task_span_update",
                    }));
                    let frame = StreamFrame::StatusUpdate(update);
                    self.record("step", &frame).await;
                    self.emit(frame);
                }
                Ok(AgentEvent::ToolCall {
                    id,
                    name,
                    arguments,
                    result,
                }) => {
                    let _ = self
                        .state
                        .sessions
                        .insert_item(&ItemRow {
                            id: new_id("item"),
                            principal_id: self.principal.id.clone(),
                            session_id: self.context_id.clone(),
                            turn_id: self.turn_id.clone(),
                            kind: "function_call".into(),
                            role: None,
                            status: "completed".into(),
                            content: json!({
                                "call_id": id,
                                "name": name,
                                "arguments": arguments,
                                "output": result.content,
                                "is_error": result.is_error,
                            }),
                            seq: self.next_seq(),
                            created_at: now_ms(),
                        })
                        .await;
                    let mut update = TaskStatusUpdateEvent::progress(
                        self.task_id.clone(),
                        self.context_id.clone(),
                        TaskStatus::now(TaskState::Working),
                    );
                    update.metadata = Some(json!({
                        "tool": name,
                        "callId": id,
                        "event": "task_span_update",
                    }));
                    let frame = StreamFrame::StatusUpdate(update);
                    self.record("step", &frame).await;
                    self.emit(frame);
                }
                // Approvals are reported through `input-required`, and usage is
                // an accounting detail: neither is an A2A frame.
                Ok(AgentEvent::Approval { .. }) | Ok(AgentEvent::Usage { .. }) => {}
                Err(e) => {
                    self.fail(&e.to_string()).await;
                    return;
                }
            }
        }
        self.complete(collected, streamed).await;
    }

    fn next_seq(&self) -> i32 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn emit(&self, frame: StreamFrame) {
        let _ = self.frames.send(frame);
    }

    /// Persist a coarse frame for replay. A failure to write the log must not
    /// stop the run: the live stream is still correct.
    async fn record(&self, kind: &str, frame: &StreamFrame) {
        let payload = json!(frame);
        let _ = self
            .store
            .append_event(&self.principal, &self.task_id, kind, &payload)
            .await;
    }

    async fn transition(&self, next: TaskState, status: Option<Value>) -> Option<A2aTaskRow> {
        match self
            .store
            .transition(&self.principal, &self.task_id, next, status, now_ms())
            .await
        {
            Ok(row) => row,
            Err(e) => {
                tracing::warn!(task_id = %self.task_id, state = %next, error = ?e, "a2a transition failed");
                None
            }
        }
    }

    async fn complete(&self, text: String, streamed: bool) {
        let artifact = final_artifact(&text);
        let _ = self
            .store
            .add_artifact(&self.principal, &self.task_id, &artifact, now_ms())
            .await;
        let status = TaskStatus::with_message(TaskState::Completed, text.clone());
        // A cancel that landed while the answer was being written owns the
        // outcome: this CAS fails and no terminal frame is published, but the
        // session and the runtime are still released below.
        let settled = self
            .transition(TaskState::Completed, Some(json!(status.clone())))
            .await;
        if let Some(row) = &settled {
            // Only a run that streamed nothing needs the whole answer as one
            // chunk; repeating it would duplicate what the client already
            // accumulated.
            if !streamed && !text.is_empty() {
                self.emit(StreamFrame::ArtifactUpdate(TaskArtifactUpdateEvent::chunk(
                    self.task_id.clone(),
                    self.context_id.clone(),
                    artifact,
                    true,
                    true,
                )));
            }
            let frame = StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
                self.task_id.clone(),
                self.context_id.clone(),
                status,
            ));
            self.record("status", &frame).await;
            self.emit(frame);
            self.notify(row, "completed").await;
        }
        let outcome = if settled.is_some() {
            "completed"
        } else {
            "canceled"
        };
        self.finish_session(&text, outcome).await;
        self.cleanup().await;
    }

    async fn fail(&self, reason: &str) {
        let status = TaskStatus::with_message(TaskState::Failed, reason);
        if let Some(row) = self
            .transition(TaskState::Failed, Some(json!(status)))
            .await
        {
            let frame = StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
                self.task_id.clone(),
                self.context_id.clone(),
                status,
            ));
            self.record("status", &frame).await;
            self.emit(frame);
            self.finish_session(reason, "failed").await;
            self.notify(&row, "failed").await;
        }
        self.cleanup().await;
    }

    /// Mirror the terminal assistant message and close the turn.
    async fn finish_session(&self, text: &str, status: &str) {
        let _ = self
            .state
            .sessions
            .insert_item(&ItemRow {
                id: new_id("item"),
                principal_id: self.principal.id.clone(),
                session_id: self.context_id.clone(),
                turn_id: self.turn_id.clone(),
                kind: "message".into(),
                role: Some("assistant".into()),
                status: if status == "completed" {
                    "completed".to_string()
                } else {
                    "incomplete".to_string()
                },
                content: Value::String(text.to_string()),
                seq: self.next_seq(),
                created_at: now_ms(),
            })
            .await;
        let _ = self
            .state
            .sessions
            .finish_turn(&self.turn_id, status, Some(text))
            .await;
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

    async fn cleanup(&self) {
        self.runtime.detach_hitl(&self.context_id).await;
        self.runtime.finish(&self.task_id).await;
    }
}

fn token_artifact(text: &str) -> Artifact {
    let mut artifact = Artifact::text(FINAL_ARTIFACT, 0, text);
    artifact.artifact_id = FINAL_ARTIFACT.to_string();
    artifact
}

fn final_artifact(text: &str) -> Artifact {
    let mut artifact = token_artifact(text);
    artifact.name = Some("answer".to_string());
    artifact.description = Some("final answer".to_string());
    artifact
}

/// Project a stored row onto the wire.
pub fn task_from(row: &A2aTaskRow, history: Option<Vec<Message>>) -> Task {
    let status = row
        .status
        .as_ref()
        .and_then(|value| serde_json::from_value::<TaskStatus>(value.clone()).ok())
        .unwrap_or_else(|| TaskStatus::now(row.state));
    Task {
        kind: "task".into(),
        id: row.task_id.clone(),
        context_id: row.context_id.clone(),
        session_id: None,
        status,
        artifacts: row.artifacts.clone(),
        history,
        metadata: None,
    }
}

/// Message history from the event log, newest last, trimmed to `length`.
pub async fn load_history(
    store: &Arc<dyn A2aTaskStore>,
    principal: &Principal,
    task_id: &str,
    length: Option<u32>,
) -> Vec<Message> {
    let events = store
        .events_since(principal, task_id, 0, 500)
        .await
        .unwrap_or_default();
    let mut messages: Vec<Message> = events
        .iter()
        .filter(|event| event.kind == "message")
        .filter_map(|event| serde_json::from_value::<Message>(event.payload.clone()).ok())
        .collect();
    if let Some(length) = length {
        let keep = length as usize;
        if messages.len() > keep {
            messages = messages.split_off(messages.len() - keep);
        }
    }
    messages
}

/// Why `cancel`/`resume` could not be applied.
#[derive(Debug)]
pub enum TaskFailure {
    NotFound,
    NotCancelable,
    NotResumable,
    /// The answer does not fit the pending request (text for an approval).
    Mismatch,
    Store(AppError),
}

/// `tasks/cancel`: CAS the terminal state, abort the run, evict the session's
/// harness runtime (the container with it) and tell the subscribers.
pub async fn cancel(
    state: &AppState,
    principal: &Principal,
    task_id: &str,
) -> Result<Task, TaskFailure> {
    let current = state
        .a2a
        .tasks
        .get(principal, task_id)
        .await
        .map_err(map_store)?;
    if current.state.is_terminal() {
        return Err(TaskFailure::NotCancelable);
    }
    let status = TaskStatus::with_message(TaskState::Canceled, "canceled by the caller");
    let row = match state
        .a2a
        .tasks
        .transition(
            principal,
            task_id,
            TaskState::Canceled,
            Some(json!(status)),
            now_ms(),
        )
        .await
    {
        Ok(Some(row)) => row,
        Ok(None) => return Err(TaskFailure::NotCancelable),
        Err(e) => return Err(TaskFailure::Store(e)),
    };
    // Release a parked decision first: aborting the future mid-`.await` is
    // harsher than letting the seam call return "nobody answered", and the CAS
    // above already guarantees the outcome cannot be overwritten.
    if let Some(hitl) = state.a2a.runtime.hitl_for(&row.context_id).await {
        hitl.abandon().await;
    }
    state.a2a.runtime.abort(task_id).await;
    // The harness runtime outlives the Rust future: for dsh it *is* the
    // container. Evict it (a no-op on the native path).
    if let Some(cancel) = &state.cancel_harness {
        cancel(principal.clone(), row.context_id.clone()).await;
    }
    let frame = StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
        task_id.to_string(),
        row.context_id.clone(),
        status,
    ));
    state.a2a.runtime.broadcast(task_id, frame.clone()).await;
    let _ = state
        .a2a
        .tasks
        .append_event(principal, task_id, "status", &json!(frame))
        .await;
    state.a2a.runtime.detach_hitl(&row.context_id).await;
    state.a2a.runtime.finish(task_id).await;
    if let Some(config) = &row.push {
        let body = json!({
            "taskId": row.task_id,
            "contextId": row.context_id,
            "state": "canceled",
            "status": row.status,
            "config": config,
        });
        let _ = state.a2a.push.notify(config, &body).await;
    }
    let messages = load_history(&state.a2a.tasks, principal, task_id, Some(20)).await;
    Ok(task_from(&row, Some(messages)))
}

/// `tasks/resume`: answer the parked approval/question and move back to working.
pub async fn resume(
    state: &AppState,
    principal: &Principal,
    task_id: &str,
    input: ResumeInput,
) -> Result<Task, TaskFailure> {
    let row = state
        .a2a
        .tasks
        .get(principal, task_id)
        .await
        .map_err(map_store)?;
    if row.state != TaskState::InputRequired {
        return Err(TaskFailure::NotResumable);
    }
    let Some(hitl) = state.a2a.runtime.hitl_for(&row.context_id).await else {
        return Err(TaskFailure::NotResumable);
    };
    match hitl.resume(input).await {
        Ok(_) => {}
        Err(ResumeError::NotParked) => return Err(TaskFailure::NotResumable),
        Err(ResumeError::Mismatch) => return Err(TaskFailure::Mismatch),
    }
    let row = state
        .a2a
        .tasks
        .get(principal, task_id)
        .await
        .map_err(TaskFailure::Store)?;
    let messages = load_history(&state.a2a.tasks, principal, task_id, Some(20)).await;
    Ok(task_from(&row, Some(messages)))
}

fn map_store(e: AppError) -> TaskFailure {
    match e {
        AppError::NotFound => TaskFailure::NotFound,
        other => TaskFailure::Store(other),
    }
}
