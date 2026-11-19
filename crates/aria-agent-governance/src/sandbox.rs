//! Quota-enforcing sandbox decorator.
//!
//! `spawn` and `restore` both go through [`MeteringSeam::reserve`], and both roll
//! the sandbox back if the reservation is refused. Because the decorator wraps
//! *any* backend, no provider can accidentally implement restore without the
//! quota gate (Runta's restore path is the precedent for why this matters).

use agent_core::seams::{MeteringSeam, ResourceRequest};
use agent_sandbox::{
    CheckpointRef, ExecOutput, ExecSpec, Sandbox, SandboxCapabilities, SandboxError, SandboxHandle,
    SandboxLifecycle,
};
use std::sync::Arc;

/// Wraps a sandbox backend and enforces one quota policy across create/restore.
pub struct QuotaGuardSandbox {
    inner: Arc<dyn Sandbox>,
    metering: Arc<dyn MeteringSeam>,
    principal_id: String,
    request: ResourceRequest,
}

impl QuotaGuardSandbox {
    pub fn new(
        inner: Arc<dyn Sandbox>,
        metering: Arc<dyn MeteringSeam>,
        principal_id: impl Into<String>,
        request: ResourceRequest,
    ) -> Self {
        Self {
            inner,
            metering,
            principal_id: principal_id.into(),
            request,
        }
    }

    async fn admit(&self, handle: &SandboxHandle) -> Result<(), SandboxError> {
        match self
            .metering
            .reserve(&self.principal_id, &handle.id, self.request)
            .await
        {
            Ok(()) => Ok(()),
            Err(e) => {
                // Roll back so a refused reservation cannot leak a running
                // container (which would bill without a reservation). The
                // destroy future must be awaited — dropping it would leak.
                if let Err(rollback) = self
                    .inner
                    .destroy(SandboxHandle {
                        id: handle.id.clone(),
                    })
                    .await
                {
                    tracing::warn!(error = %rollback, "could not roll back a refused sandbox");
                }
                Err(SandboxError::Spawn(format!("quota: {e}")))
            }
        }
    }
}

#[async_trait::async_trait]
impl Sandbox for QuotaGuardSandbox {
    async fn spawn(&self, spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        let handle = self.inner.spawn(spec).await?;
        self.admit(&handle).await?;
        Ok(handle)
    }

    async fn exec(
        &self,
        handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        self.inner.exec(handle, cmd).await
    }

    async fn destroy(&self, handle: SandboxHandle) -> Result<(), SandboxError> {
        let id = handle.id.clone();
        let result = self.inner.destroy(handle).await;
        let _ = self.metering.release(&self.principal_id, &id).await;
        result
    }

    fn capabilities(&self) -> SandboxCapabilities {
        self.inner.capabilities()
    }

    async fn status(&self, handle: &SandboxHandle) -> Result<SandboxLifecycle, SandboxError> {
        self.inner.status(handle).await
    }

    async fn pause(&self, handle: &SandboxHandle) -> Result<(), SandboxError> {
        self.inner.pause(handle).await
    }

    async fn resume(&self, handle: &SandboxHandle) -> Result<(), SandboxError> {
        self.inner.resume(handle).await
    }

    async fn suspend(&self, handle: &SandboxHandle) -> Result<(), SandboxError> {
        self.inner.suspend(handle).await
    }

    async fn checkpoint(&self, handle: &SandboxHandle) -> Result<CheckpointRef, SandboxError> {
        self.inner.checkpoint(handle).await
    }

    async fn restore(
        &self,
        checkpoint: &CheckpointRef,
        spec: &ExecSpec,
    ) -> Result<SandboxHandle, SandboxError> {
        let handle = self.inner.restore(checkpoint, spec).await?;
        self.admit(&handle).await?;
        Ok(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metering::{MeteringLedger, QuotaPolicy};
    use agent_sandbox::ExecOutput;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeSandbox {
        destroyed: AtomicUsize,
        next: AtomicUsize,
        fail_destroy: bool,
    }

    impl FakeSandbox {
        fn new() -> Self {
            Self {
                destroyed: AtomicUsize::new(0),
                next: AtomicUsize::new(1),
                fail_destroy: false,
            }
        }

        /// A backend whose teardown fails: rollback cannot undo the spawn.
        fn broken() -> Self {
            Self {
                fail_destroy: true,
                ..Self::new()
            }
        }

        fn id(&self) -> String {
            format!("sbx-{}", self.next.fetch_add(1, Ordering::SeqCst))
        }
    }

    #[async_trait::async_trait]
    impl Sandbox for FakeSandbox {
        async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
            Ok(SandboxHandle { id: self.id() })
        }
        async fn exec(
            &self,
            _handle: &SandboxHandle,
            _cmd: &[String],
        ) -> Result<ExecOutput, SandboxError> {
            Ok(ExecOutput {
                exit_code: 0,
                stdout: "ok".into(),
                stderr: String::new(),
            })
        }
        async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
            if self.fail_destroy {
                return Err(SandboxError::Exec("already gone".into()));
            }
            self.destroyed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        async fn checkpoint(&self, handle: &SandboxHandle) -> Result<CheckpointRef, SandboxError> {
            Ok(CheckpointRef::new("img:latest", handle.id.clone(), 1))
        }
        async fn restore(
            &self,
            _checkpoint: &CheckpointRef,
            _spec: &ExecSpec,
        ) -> Result<SandboxHandle, SandboxError> {
            Ok(SandboxHandle { id: self.id() })
        }
    }

    fn request() -> ResourceRequest {
        ResourceRequest {
            vcpus_x1000: 2000,
            memory_mib: 1024,
            disk_mib: 10_240,
        }
    }

    /// Rollback can fail on its own (the container is already gone); the quota
    /// refusal must still reach the caller.
    #[tokio::test]
    async fn a_refused_reservation_still_reports_when_rollback_fails() {
        let ledger = Arc::new(MeteringLedger::new(None, Some(QuotaPolicy::new(0))));
        let guarded =
            QuotaGuardSandbox::new(Arc::new(FakeSandbox::broken()), ledger, "p", request());
        let err = guarded
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap_err();
        assert!(matches!(err, SandboxError::Spawn(_)), "{err:?}");
    }

    #[tokio::test]
    async fn spawn_reserves_and_destroy_releases() {
        let inner = Arc::new(FakeSandbox::new());
        let ledger = Arc::new(MeteringLedger::new(None, Some(QuotaPolicy::new(10_000))));
        let guarded = QuotaGuardSandbox::new(inner.clone(), ledger.clone(), "p", request());
        let handle = guarded
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap();
        assert_eq!(ledger.reserved_weight("p"), request().weight());
        guarded.destroy(handle).await.unwrap();
        assert_eq!(ledger.reserved_weight("p"), 0);
        assert_eq!(inner.destroyed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_refused_reservation_rolls_the_sandbox_back() {
        let inner = Arc::new(FakeSandbox::new());
        let ledger = Arc::new(MeteringLedger::new(None, Some(QuotaPolicy::new(1_000))));
        let guarded = QuotaGuardSandbox::new(inner.clone(), ledger, "p", request());
        let err = guarded
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");
        assert_eq!(
            inner.destroyed.load(Ordering::SeqCst),
            1,
            "a refused sandbox must not keep billing"
        );
    }

    #[tokio::test]
    async fn restore_takes_the_same_gate_as_spawn() {
        let inner = Arc::new(FakeSandbox::new());
        // One sandbox fits (4048 weight), a second one does not (8096 > 5000).
        let ledger = Arc::new(MeteringLedger::new(None, Some(QuotaPolicy::new(5_000))));
        let guarded = QuotaGuardSandbox::new(inner.clone(), ledger.clone(), "p", request());
        let origin = guarded
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap();
        let checkpoint = guarded.checkpoint(&origin).await.unwrap();

        let err = guarded
            .restore(&checkpoint, &ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");
        assert_eq!(ledger.reserved_weight("p"), request().weight());
        assert_eq!(inner.destroyed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn optional_capabilities_pass_through() {
        let inner = Arc::new(FakeSandbox::new());
        let guarded = QuotaGuardSandbox::new(
            inner.clone(),
            Arc::new(MeteringLedger::default()),
            "p",
            request(),
        );
        assert!(!guarded.capabilities().pause);
        let handle = guarded
            .spawn(&ExecSpec::command(vec!["true".into()]))
            .await
            .unwrap();
        assert!(guarded.pause(&handle).await.is_err(), "delegated default");
        assert_eq!(
            guarded
                .exec(&handle, &["true".into()])
                .await
                .unwrap()
                .stdout,
            "ok"
        );
        assert!(guarded.status(&handle).await.is_err());
        assert!(guarded.resume(&handle).await.is_err());
        assert!(guarded.suspend(&handle).await.is_err());
    }
}
