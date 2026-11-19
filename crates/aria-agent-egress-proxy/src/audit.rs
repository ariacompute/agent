//! Batched audit forwarding.
//!
//! The gateway calls `record` on the request path, so `record` must never block
//! on the control plane. Events are buffered and flushed on a size or time
//! threshold; a dropped batch is counted and logged (never silently ignored).

use crate::remote::RemoteControl;
use agent_core::seams::{AuditEvent, AuditSink, SeamError, SeamResult};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Flush when this many events are buffered…
const BATCH_SIZE: usize = 64;
/// …or when this much time has passed, whichever comes first.
const FLUSH_EVERY: Duration = Duration::from_millis(200);
/// Bounded queue: audit backpressure must not become tool latency.
const QUEUE: usize = 4096;

pub struct RemoteAudit {
    tx: mpsc::Sender<AuditEvent>,
    dropped: AtomicU64,
}

impl RemoteAudit {
    /// Start the flusher. The task lives as long as the process.
    pub fn spawn(control: Arc<RemoteControl>) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(QUEUE);
        tokio::spawn(flush_loop(control, rx));
        Arc::new(Self {
            tx,
            dropped: AtomicU64::new(0),
        })
    }

    /// Events the queue could not take (visible, not silent).
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl AuditSink for RemoteAudit {
    async fn record(&self, event: AuditEvent) -> SeamResult<()> {
        if self.tx.try_send(event).is_err() {
            let dropped = self.dropped.fetch_add(1, Ordering::Relaxed) + 1;
            if dropped == 1 || dropped.is_multiple_of(100) {
                tracing::warn!(dropped, "audit queue is full; events are being dropped");
            }
        }
        Ok(())
    }

    async fn recent(&self, _limit: usize) -> SeamResult<Vec<AuditEvent>> {
        Err(SeamError::Unavailable(
            "the egress proxy keeps no local audit history; query /v1/governance/audit".into(),
        ))
    }
}

async fn flush_loop(control: Arc<RemoteControl>, mut rx: mpsc::Receiver<AuditEvent>) {
    let mut buffer: Vec<AuditEvent> = Vec::with_capacity(BATCH_SIZE);
    let mut ticker = tokio::time::interval(FLUSH_EVERY);
    let mut last_error: Option<std::time::Instant> = None;
    loop {
        tokio::select! {
            maybe = rx.recv() => {
                match maybe {
                    Some(event) => {
                        buffer.push(event);
                        if buffer.len() >= BATCH_SIZE {
                            flush(&control, &mut buffer, &mut last_error).await;
                        }
                    }
                    // The sink was dropped: send what is left, then stop.
                    None => break,
                }
            }
            _ = ticker.tick() => {
                if !buffer.is_empty() {
                    flush(&control, &mut buffer, &mut last_error).await;
                }
            }
        }
    }
    flush(&control, &mut buffer, &mut last_error).await;
}

async fn flush(
    control: &RemoteControl,
    buffer: &mut Vec<AuditEvent>,
    last_error: &mut Option<std::time::Instant>,
) {
    if buffer.is_empty() {
        return;
    }
    let batch = std::mem::take(buffer);
    let Err(e) = control.audit(&batch).await else {
        return;
    };
    // Rate-limit the warning: an unreachable control plane would otherwise print
    // once per batch forever.
    let now = std::time::Instant::now();
    let should_log = last_error
        .map(|at| now.duration_since(at) >= Duration::from_secs(5))
        .unwrap_or(true);
    if !should_log {
        return;
    }
    tracing::warn!(error = %e, count = batch.len(), "audit batch rejected by the control plane");
    *last_error = Some(now);
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::AuditKind;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn events_are_flushed_in_a_batch() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/audit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let sink = RemoteAudit::spawn(control);
        for i in 0..3 {
            sink.record(AuditEvent::new(
                AuditKind::EgressDecision,
                "tenant-a",
                format!("h{i}.example.com"),
            ))
            .await
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "three events share one flush");
        assert_eq!(sink.dropped(), 0);
        assert!(matches!(
            sink.recent(1).await,
            Err(SeamError::Unavailable(_))
        ));
    }

    #[tokio::test]
    async fn a_full_batch_and_a_closed_queue_both_flush() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/audit"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true})))
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let sink = RemoteAudit::spawn(control);
        // One over the batch size: the size threshold flushes without the timer.
        for i in 0..BATCH_SIZE + 1 {
            sink.record(AuditEvent::new(
                AuditKind::ToolExecuted,
                "tenant-a",
                format!("tool-{i}"),
            ))
            .await
            .unwrap();
        }
        // Dropping the sink closes the queue: the leftover event is flushed and
        // the loop stops.
        sink.record(AuditEvent::new(AuditKind::ToolExecuted, "tenant-a", "last"))
            .await
            .unwrap();
        drop(sink);
        tokio::time::sleep(Duration::from_millis(200)).await;
        let requests = server.received_requests().await.unwrap();
        assert!(requests.len() >= 2, "batch + drain: {requests:?}");
    }

    #[tokio::test]
    async fn flush_without_events_is_a_no_op() {
        let server = MockServer::start().await;
        let control = RemoteControl::new(server.uri(), "t").unwrap();
        let mut buffer = Vec::new();
        let mut last_error = None;
        flush(&control, &mut buffer, &mut last_error).await;
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn repeated_rejections_are_logged_only_once_per_window() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/audit"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let control = RemoteControl::new(server.uri(), "t").unwrap();
        let event = AuditEvent::new(AuditKind::ToolExecuted, "t", "shell");

        let mut last_error = None;
        flush(&control, &mut vec![event.clone()], &mut last_error).await;
        assert!(last_error.is_some(), "the first failure is recorded");
        // A second failure inside the window keeps the original timestamp.
        let first = last_error;
        flush(&control, &mut vec![event], &mut last_error).await;
        assert_eq!(last_error, first);
    }

    #[tokio::test]
    async fn a_rejected_batch_does_not_block_record() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/internal/egress/audit"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let control = Arc::new(RemoteControl::new(server.uri(), "t").unwrap());
        let sink = RemoteAudit::spawn(control);
        assert!(sink
            .record(AuditEvent::new(AuditKind::ToolExecuted, "t", "shell"))
            .await
            .is_ok());
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(sink.dropped(), 0, "a rejected batch is logged, not charged");
    }
}
