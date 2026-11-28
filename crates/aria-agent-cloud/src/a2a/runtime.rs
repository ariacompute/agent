//! In-process registry of the A2A tasks this process is running.
//!
//! Three maps, one lock, and a rule that makes them safe to reason about:
//!
//! * `tasks` — the frame channel and the join handle of each live run, so a
//!   stream subscriber can attach and `tasks/cancel` can abort;
//! * `contexts` — one live task per context. A context is one session and, with
//!   the default dsh harness, one container: two concurrent turns in it would
//!   interleave prompts and approvals;
//! * `hitl` — session → the parked task's channel. dsh's approval/question
//!   requests arrive on a per-tenant handler, so the session id injected by
//!   `dsh::SessionTag` is what points them at the right task.
//!
//! Everything is dropped by [`A2aRuntime::finish`], including the context lock,
//! so a completed task never blocks the next turn in the same session.

use std::collections::HashMap;
use std::sync::Arc;

use aria_agent_a2a::types::StreamFrame;
use tokio::sync::{broadcast, RwLock};

use super::hitl::PausingHitl;

/// Frames buffered per task before a slow subscriber starts lagging.
pub const FRAME_CAPACITY: usize = 256;

struct TaskEntry {
    frames: broadcast::Sender<StreamFrame>,
    join: Option<tokio::task::JoinHandle<()>>,
}

#[derive(Default)]
struct Inner {
    tasks: HashMap<String, TaskEntry>,
    contexts: HashMap<String, String>,
    hitl: HashMap<String, Arc<PausingHitl>>,
}

/// The channel pair handed to the run task and to the HTTP handler.
pub struct TaskChannel {
    pub frames: broadcast::Sender<StreamFrame>,
    pub receiver: broadcast::Receiver<StreamFrame>,
}

/// Another live task already owns the context (a second turn in the same
/// session). The caller answers `invalid_state_transition` rather than
/// interleaving two runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextBusy;

/// Registry of live tasks.
#[derive(Default)]
pub struct A2aRuntime {
    inner: RwLock<Inner>,
}

impl A2aRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reserve `context_id` for `task_id` and open the frame channel.
    pub async fn open(&self, task_id: &str, context_id: &str) -> Result<TaskChannel, ContextBusy> {
        let mut inner = self.inner.write().await;
        if let Some(owner) = inner.contexts.get(context_id) {
            if owner != task_id {
                return Err(ContextBusy);
            }
        }
        let (frames, receiver) = broadcast::channel(FRAME_CAPACITY);
        inner.tasks.insert(
            task_id.to_string(),
            TaskEntry {
                frames: frames.clone(),
                join: None,
            },
        );
        inner
            .contexts
            .insert(context_id.to_string(), task_id.to_string());
        Ok(TaskChannel { frames, receiver })
    }

    /// Record the spawned run so `cancel` can abort it.
    pub async fn set_join(&self, task_id: &str, join: tokio::task::JoinHandle<()>) {
        if let Some(entry) = self.inner.write().await.tasks.get_mut(task_id) {
            entry.join = Some(join);
        }
    }

    /// Attach a new subscriber, or `None` when the task is not live any more.
    pub async fn subscribe(&self, task_id: &str) -> Option<broadcast::Receiver<StreamFrame>> {
        self.inner
            .read()
            .await
            .tasks
            .get(task_id)
            .map(|entry| entry.frames.subscribe())
    }

    /// Publish one frame on the task's channel. `false` when no handle is
    /// registered: a cancel that lands after the run task cleaned up has nobody
    /// left to tell, and that is not an error.
    pub async fn broadcast(&self, task_id: &str, frame: StreamFrame) -> bool {
        let sender = self
            .inner
            .read()
            .await
            .tasks
            .get(task_id)
            .map(|entry| entry.frames.clone());
        match sender {
            Some(sender) => sender.send(frame).is_ok(),
            None => false,
        }
    }

    /// Abort the run task. `false` when it had already finished or was never
    /// registered (the caller still writes the terminal state).
    pub async fn abort(&self, task_id: &str) -> bool {
        let inner = self.inner.read().await;
        match inner
            .tasks
            .get(task_id)
            .and_then(|entry| entry.join.as_ref())
        {
            Some(join) => {
                join.abort();
                true
            }
            None => false,
        }
    }

    /// Forget the task and release its context.
    pub async fn finish(&self, task_id: &str) {
        let mut inner = self.inner.write().await;
        inner.tasks.remove(task_id);
        inner.contexts.retain(|_, owner| owner != task_id);
    }

    /// Whether a run is still registered.
    pub async fn is_active(&self, task_id: &str) -> bool {
        self.inner.read().await.tasks.contains_key(task_id)
    }

    /// The live task holding `context_id`, if any.
    pub async fn active_task_for(&self, context_id: &str) -> Option<String> {
        self.inner.read().await.contexts.get(context_id).cloned()
    }

    /// Number of live tasks (diagnostics / tests).
    pub async fn live_tasks(&self) -> usize {
        self.inner.read().await.tasks.len()
    }

    /// Route dsh's approval / question requests for `session` to `hitl`.
    pub async fn attach_hitl(&self, session: &str, hitl: Arc<PausingHitl>) {
        self.inner
            .write()
            .await
            .hitl
            .insert(session.to_string(), hitl);
    }

    /// Stop routing for `session` (terminal or cancelled task).
    pub async fn detach_hitl(&self, session: &str) {
        self.inner.write().await.hitl.remove(session);
    }

    /// The parked task waiting on `session`, if any.
    pub async fn hitl_for(&self, session: &str) -> Option<Arc<PausingHitl>> {
        self.inner.read().await.hitl.get(session).cloned()
    }
}
