//! In-process sub-agents (`subagent_*`).
//!
//! A child gets its own session id namespaced under the parent
//! (`<parent>::sub::<id>`), so the parent can always trace and replay it while
//! the child's turns stay out of the parent's own event stream (ADR-0017).
//!
//! The provider never invents work: without a factory it does not exist, and a
//! child that fails reports the failure instead of an empty answer.

use agent_core::seams::{SeamError, SeamResult, SubagentHandle, SubagentRequest, Subagents};
use async_trait::async_trait;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Separator between the parent session and a child session id.
///
/// Defined in `agent_core` so the loop and the cloud can walk a child session id
/// back to its parent without depending on this crate.
pub use agent_core::seams::CHILD_SESSION_SEP;

/// Default cap on concurrently running children.
pub const DEFAULT_MAX_CONCURRENT: usize = 4;

/// Runs one child agent to completion.
#[async_trait]
pub trait SubagentFactory: Send + Sync {
    /// `session` is the child's own session id (already namespaced).
    async fn run(&self, req: &SubagentRequest, session: &str) -> SeamResult<String>;
}

struct Child {
    handle: SubagentHandle,
    outcome: Option<SeamResult<String>>,
    task: Option<tokio::task::JoinHandle<SeamResult<String>>>,
}

/// Provider that forks children inside the current process.
pub struct InProcessSubagents {
    factory: Arc<dyn SubagentFactory>,
    max_concurrent: usize,
    children: Mutex<BTreeMap<String, Child>>,
}

impl InProcessSubagents {
    pub fn new(factory: Arc<dyn SubagentFactory>) -> Self {
        Self {
            factory,
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            children: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn with_max_concurrent(mut self, max: usize) -> Self {
        self.max_concurrent = max.max(1);
        self
    }

    pub fn max_concurrent(&self) -> usize {
        self.max_concurrent
    }

    /// Number of children that have not finished yet.
    pub fn running(&self) -> SeamResult<usize> {
        Ok(self
            .lock()?
            .values()
            .filter(|c| c.outcome.is_none())
            .count())
    }

    fn lock(&self) -> SeamResult<std::sync::MutexGuard<'_, BTreeMap<String, Child>>> {
        self.children
            .lock()
            .map_err(|e| SeamError::Io(format!("subagent registry poisoned: {e}")))
    }
}

#[async_trait]
impl Subagents for InProcessSubagents {
    async fn spawn(&self, req: SubagentRequest) -> SeamResult<SubagentHandle> {
        if req.task.trim().is_empty() {
            return Err(SeamError::Invalid("subagent task must not be empty".into()));
        }
        if req.parent_session.trim().is_empty() {
            return Err(SeamError::Invalid(
                "subagent parent session must not be empty".into(),
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let session = agent_core::seams::child_session(&req.parent_session, &id);
        let handle = SubagentHandle {
            id: id.clone(),
            session: session.clone(),
            parent_session: req.parent_session.clone(),
        };

        let mut guard = self.lock()?;
        let running = guard.values().filter(|c| c.outcome.is_none()).count();
        if running >= self.max_concurrent {
            return Err(SeamError::Denied(format!(
                "subagent concurrency limit reached ({})",
                self.max_concurrent
            )));
        }
        let factory = self.factory.clone();
        let child_req = req.clone();
        let child_session = session.clone();
        let task = tokio::spawn(async move { factory.run(&child_req, &child_session).await });
        guard.insert(
            id,
            Child {
                handle: handle.clone(),
                outcome: None,
                task: Some(task),
            },
        );
        Ok(handle)
    }

    async fn list(&self, parent_session: &str) -> SeamResult<Vec<SubagentHandle>> {
        let guard = self.lock()?;
        Ok(guard
            .values()
            .filter(|c| c.handle.parent_session == parent_session)
            .map(|c| c.handle.clone())
            .collect())
    }

    async fn wait(&self, id: &str) -> SeamResult<String> {
        let task = {
            let mut guard = self.lock()?;
            let child = guard
                .get_mut(id)
                .ok_or_else(|| SeamError::NotFound(format!("subagent {id}")))?;
            if let Some(outcome) = &child.outcome {
                return outcome.clone();
            }
            child
                .task
                .take()
                .ok_or_else(|| SeamError::Io(format!("subagent {id} has no pending task")))?
        };
        let outcome = match task.await {
            Ok(result) => result,
            Err(e) => Err(SeamError::Io(format!("subagent {id} failed: {e}"))),
        };
        // Cache the outcome so `wait` is idempotent and the slot is freed.
        let mut guard = self.lock()?;
        if let Some(child) = guard.get_mut(id) {
            child.outcome = Some(outcome.clone());
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Factory that echoes the task and counts invocations.
    struct EchoFactory {
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl SubagentFactory for EchoFactory {
        async fn run(&self, req: &SubagentRequest, session: &str) -> SeamResult<String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(format!("[{}] {}", session, req.task))
        }
    }

    fn provider() -> (InProcessSubagents, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        (
            InProcessSubagents::new(Arc::new(EchoFactory {
                calls: calls.clone(),
            })),
            calls,
        )
    }

    #[tokio::test]
    async fn spawn_runs_the_child_in_its_own_session() {
        let (subagents, calls) = provider();
        let handle = subagents
            .spawn(SubagentRequest::new("parent", "summarise the repo"))
            .await
            .unwrap();
        assert!(handle.session.starts_with("parent::sub::"));
        assert_eq!(handle.parent_session, "parent");
        let out = subagents.wait(&handle.id).await.unwrap();
        assert!(out.contains("summarise the repo"), "{out}");
        assert!(out.contains(&handle.session));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Waiting twice returns the cached outcome without re-running.
        assert_eq!(subagents.wait(&handle.id).await.unwrap(), out);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn list_is_scoped_to_the_parent_session() {
        let (subagents, _) = provider();
        let a = subagents
            .spawn(SubagentRequest::new("p1", "a"))
            .await
            .unwrap();
        let b = subagents
            .spawn(SubagentRequest::new("p2", "b"))
            .await
            .unwrap();
        let listed = subagents.list("p1").await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, a.id);
        assert!(subagents.list("p2").await.unwrap()[0].id == b.id);
        assert!(subagents.list("nobody").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn empty_requests_and_unknown_ids_are_errors() {
        let (subagents, _) = provider();
        assert!(matches!(
            subagents.spawn(SubagentRequest::new("p", "   ")).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            subagents.spawn(SubagentRequest::new("", "task")).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            subagents.wait("missing").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn concurrency_limit_is_enforced_and_released() {
        struct Slow;

        #[async_trait]
        impl SubagentFactory for Slow {
            async fn run(&self, _req: &SubagentRequest, _session: &str) -> SeamResult<String> {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                Ok("done".into())
            }
        }

        let subagents = InProcessSubagents::new(Arc::new(Slow)).with_max_concurrent(1);
        assert_eq!(subagents.max_concurrent(), 1);
        let first = subagents
            .spawn(SubagentRequest::new("p", "one"))
            .await
            .unwrap();
        assert_eq!(subagents.running().unwrap(), 1);
        assert!(matches!(
            subagents.spawn(SubagentRequest::new("p", "two")).await,
            Err(SeamError::Denied(_))
        ));
        subagents.wait(&first.id).await.unwrap();
        assert_eq!(subagents.running().unwrap(), 0);
        // The slot is free again.
        subagents
            .spawn(SubagentRequest::new("p", "two"))
            .await
            .unwrap();
        assert_eq!(subagents.with_max_concurrent(0).max_concurrent(), 1);
    }

    #[tokio::test]
    async fn concurrent_waits_on_one_child_report_no_pending_task() {
        struct Blocking;
        #[async_trait]
        impl SubagentFactory for Blocking {
            async fn run(&self, _req: &SubagentRequest, _session: &str) -> SeamResult<String> {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                Ok("late".into())
            }
        }
        let subagents = Arc::new(InProcessSubagents::new(Arc::new(Blocking)));
        let handle = subagents
            .spawn(SubagentRequest::new("p", "task"))
            .await
            .unwrap();

        let a = subagents.clone();
        let id = handle.id.clone();
        let first = tokio::spawn(async move { a.wait(&id).await });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        // The first waiter took the task; the second finds no pending task.
        assert!(matches!(
            subagents.wait(&handle.id).await,
            Err(SeamError::Io(_))
        ));
        assert_eq!(first.await.unwrap().unwrap(), "late");
    }

    #[tokio::test]
    async fn a_poisoned_registry_reports_io_errors() {
        let (subagents, _) = provider();
        let r = &subagents;
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = r.children.lock().unwrap();
            panic!("poison children");
        }));
        assert!(matches!(subagents.running(), Err(SeamError::Io(_))));
    }

    #[tokio::test]
    async fn a_failing_child_reports_the_failure() {
        struct Broken;

        #[async_trait]
        impl SubagentFactory for Broken {
            async fn run(&self, _req: &SubagentRequest, _session: &str) -> SeamResult<String> {
                Err(SeamError::Io("child crashed".into()))
            }
        }

        let subagents = InProcessSubagents::new(Arc::new(Broken));
        let handle = subagents
            .spawn(SubagentRequest::new("p", "do it"))
            .await
            .unwrap();
        assert!(matches!(
            subagents.wait(&handle.id).await,
            Err(SeamError::Io(_))
        ));
        // The failure is cached, not retried.
        assert!(subagents.wait(&handle.id).await.is_err());
        assert_eq!(subagents.running().unwrap(), 0);
    }
}
