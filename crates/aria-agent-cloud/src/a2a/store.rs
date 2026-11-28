//! Persistence for A2A tasks and their coarse-grained event log.
//!
//! The port lives here (in the consumer) so the memory and Postgres providers
//! are interchangeable, and so every query is written down twice next to each
//! other: once for tests, once for production.
//!
//! Two invariants the implementations must share:
//!
//! * **Tenant scoping** — every read and write filters by `principal_id` (an
//!   admin sees all, per [`crate::store::can_see`]). A task that is not visible
//!   answers `NotFound`, never "forbidden", so ids cannot be probed.
//! * **CAS transitions** — `transition` only moves a task that is still live and
//!   only along the legal table; a terminal task yields `Ok(None)`. That is what
//!   makes a late `completed` lose to a `canceled` instead of overwriting it.

use std::collections::HashMap;
use std::sync::Mutex;

use aria_agent_a2a::types::{Artifact, PushNotificationConfig};
use aria_agent_a2a::TaskState;
use async_trait::async_trait;
use serde_json::Value;

use crate::auth::Principal;
use crate::store::can_see;
use crate::AppError;

/// One persisted task.
#[derive(Debug, Clone, PartialEq)]
pub struct A2aTaskRow {
    pub task_id: String,
    pub principal_id: String,
    pub agent_id: String,
    /// Mirrors `agent_sessions.id`.
    pub context_id: String,
    /// Mirrors `session_turns.id`.
    pub turn_id: String,
    pub state: TaskState,
    /// Serialized [`aria_agent_a2a::types::TaskStatus`]; `None` before the first
    /// write.
    pub status: Option<Value>,
    pub artifacts: Vec<Artifact>,
    pub push: Option<PushNotificationConfig>,
    pub push_last: Option<Value>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

impl A2aTaskRow {
    /// A `submitted` row for a freshly accepted task.
    pub fn submitted(
        task_id: &str,
        principal_id: &str,
        agent_id: &str,
        context_id: &str,
        turn_id: &str,
        status: Value,
        now_ms: i64,
    ) -> Self {
        Self {
            task_id: task_id.to_string(),
            principal_id: principal_id.to_string(),
            agent_id: agent_id.to_string(),
            context_id: context_id.to_string(),
            turn_id: turn_id.to_string(),
            state: TaskState::Submitted,
            status: Some(status),
            artifacts: Vec::new(),
            push: None,
            push_last: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        }
    }
}

/// One append-only event. `payload` is a serialized
/// [`aria_agent_a2a::types::StreamFrame`] so replay needs no second mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct A2aEventRecord {
    pub seq: u64,
    pub kind: String,
    pub payload: Value,
}

/// Storage for A2A tasks and their event log.
#[async_trait]
pub trait A2aTaskStore: Send + Sync {
    async fn create(&self, principal: &Principal, row: &A2aTaskRow) -> Result<(), AppError>;
    async fn get(&self, principal: &Principal, task_id: &str) -> Result<A2aTaskRow, AppError>;
    /// CAS: only a live task may move, and only along the lifecycle. `Ok(None)`
    /// means the task had already reached a terminal state.
    async fn transition(
        &self,
        principal: &Principal,
        task_id: &str,
        next: TaskState,
        status: Option<Value>,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError>;
    async fn add_artifact(
        &self,
        principal: &Principal,
        task_id: &str,
        artifact: &Artifact,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError>;
    async fn append_event(
        &self,
        principal: &Principal,
        task_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<u64, AppError>;
    async fn events_since(
        &self,
        principal: &Principal,
        task_id: &str,
        since: u64,
        limit: usize,
    ) -> Result<Vec<A2aEventRecord>, AppError>;
    async fn set_push(
        &self,
        principal: &Principal,
        task_id: &str,
        config: &PushNotificationConfig,
    ) -> Result<(), AppError>;
    async fn set_push_last(
        &self,
        principal: &Principal,
        task_id: &str,
        result: &Value,
        now_ms: i64,
    ) -> Result<(), AppError>;
    /// Mark every non-terminal task `failed` (process restart). Returns how many
    /// rows were closed. Never pretends a task is still running.
    async fn fail_orphans(&self, reason: &str, now_ms: i64) -> Result<usize, AppError>;
}

/// In-memory provider: the semantics every test runs against.
#[derive(Default)]
pub struct MemoryA2aTasks {
    inner: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    tasks: HashMap<String, A2aTaskRow>,
    events: HashMap<String, Vec<A2aEventRecord>>,
    next_seq: u64,
}

impl MemoryA2aTasks {
    pub fn new() -> Self {
        Self::default()
    }

    fn visible(row: &A2aTaskRow, principal: &Principal) -> bool {
        can_see(principal, &row.principal_id)
    }
}

#[async_trait]
impl A2aTaskStore for MemoryA2aTasks {
    async fn create(&self, principal: &Principal, row: &A2aTaskRow) -> Result<(), AppError> {
        if !can_see(principal, &row.principal_id) {
            return Err(AppError::NotFound);
        }
        let mut state = self.inner.lock().expect("a2a memory store");
        if state.tasks.contains_key(&row.task_id) {
            return Err(AppError::Conflict(format!(
                "task {} already exists",
                row.task_id
            )));
        }
        state.tasks.insert(row.task_id.clone(), row.clone());
        Ok(())
    }

    async fn get(&self, principal: &Principal, task_id: &str) -> Result<A2aTaskRow, AppError> {
        let state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        Ok(row.clone())
    }

    async fn transition(
        &self,
        principal: &Principal,
        task_id: &str,
        next: TaskState,
        status: Option<Value>,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get_mut(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        if row.state.is_terminal() {
            return Ok(None);
        }
        if !row.state.can_transition(next) {
            return Err(AppError::Conflict(format!(
                "task {task_id} cannot move from {} to {}",
                row.state,
                next.as_str()
            )));
        }
        row.state = next;
        if status.is_some() {
            row.status = status;
        }
        row.updated_at_ms = now_ms;
        Ok(Some(row.clone()))
    }

    async fn add_artifact(
        &self,
        principal: &Principal,
        task_id: &str,
        artifact: &Artifact,
        now_ms: i64,
    ) -> Result<Option<A2aTaskRow>, AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get_mut(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        if row.state.is_terminal() {
            return Ok(None);
        }
        row.artifacts.push(artifact.clone());
        row.updated_at_ms = now_ms;
        Ok(Some(row.clone()))
    }

    async fn append_event(
        &self,
        principal: &Principal,
        task_id: &str,
        kind: &str,
        payload: &Value,
    ) -> Result<u64, AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        state.next_seq += 1;
        let seq = state.next_seq;
        state
            .events
            .entry(task_id.to_string())
            .or_default()
            .push(A2aEventRecord {
                seq,
                kind: kind.to_string(),
                payload: payload.clone(),
            });
        Ok(seq)
    }

    async fn events_since(
        &self,
        principal: &Principal,
        task_id: &str,
        since: u64,
        limit: usize,
    ) -> Result<Vec<A2aEventRecord>, AppError> {
        let state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        let events = state
            .events
            .get(task_id)
            .map(|events| {
                events
                    .iter()
                    .filter(|event| event.seq > since)
                    .take(limit)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        Ok(events)
    }

    async fn set_push(
        &self,
        principal: &Principal,
        task_id: &str,
        config: &PushNotificationConfig,
    ) -> Result<(), AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get_mut(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        row.push = Some(config.clone());
        Ok(())
    }

    async fn set_push_last(
        &self,
        principal: &Principal,
        task_id: &str,
        result: &Value,
        now_ms: i64,
    ) -> Result<(), AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let row = state.tasks.get_mut(task_id).ok_or(AppError::NotFound)?;
        if !Self::visible(row, principal) {
            return Err(AppError::NotFound);
        }
        row.push_last = Some(result.clone());
        row.updated_at_ms = now_ms;
        Ok(())
    }

    async fn fail_orphans(&self, reason: &str, now_ms: i64) -> Result<usize, AppError> {
        let mut state = self.inner.lock().expect("a2a memory store");
        let mut closed = 0;
        let status = serde_json::json!({
            "state": "failed",
            "message": {
                "kind": "message",
                "role": "agent",
                "parts": [{"kind": "text", "type": "text", "text": reason}],
            },
        });
        for row in state.tasks.values_mut() {
            if row.state.is_terminal() {
                continue;
            }
            row.state = TaskState::Failed;
            row.status = Some(status.clone());
            row.updated_at_ms = now_ms;
            closed += 1;
        }
        Ok(closed)
    }
}
