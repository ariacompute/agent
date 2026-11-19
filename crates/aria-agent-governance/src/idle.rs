//! Idle detection over **multiple signals**.
//!
//! Runta decides "idle" from inbound HTTP traffic alone, which means a sandbox
//! running a build, a background job or an SSH session looks idle and gets
//! suspended mid-work. Here any of five signals keeps the sandbox alive, and the
//! verdict names the signal that did it so the behaviour is debuggable.

use agent_core::seams::{now_ms, SeamResult};
use async_trait::async_trait;

/// One sample of everything that means "work is happening".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct IdleSignals {
    /// Last inbound HTTP request (proxy or published port).
    pub last_http_request_ms: u64,
    /// Last command executed through the sandbox.
    pub last_exec_ms: u64,
    /// Live processes inside the sandbox.
    pub running_processes: u32,
    /// Open sockets (inbound or outbound).
    pub open_connections: u32,
    /// Jobs queued or running in the jobs seam.
    pub pending_jobs: u32,
}

/// Why a sandbox is still considered active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdleVerdict {
    Active { reason: String },
    Idle { idle_for_ms: u64 },
}

impl IdleVerdict {
    pub fn is_idle(&self) -> bool {
        matches!(self, IdleVerdict::Idle { .. })
    }
}

impl IdleSignals {
    /// Returns the reason this sample is active, if it is.
    pub fn active_reason(&self, now: u64, timeout_ms: u64) -> Option<String> {
        if self.running_processes > 0 {
            return Some(format!("{} running process(es)", self.running_processes));
        }
        if self.open_connections > 0 {
            return Some(format!("{} open connection(s)", self.open_connections));
        }
        if self.pending_jobs > 0 {
            return Some(format!("{} pending job(s)", self.pending_jobs));
        }
        let window = |last: u64| last > 0 && now.saturating_sub(last) < timeout_ms;
        if window(self.last_exec_ms) {
            return Some("recent command execution".to_string());
        }
        if window(self.last_http_request_ms) {
            return Some("recent inbound request".to_string());
        }
        None
    }

    /// How long the oldest activity signal has been quiet.
    pub fn quiet_for_ms(&self, now: u64) -> u64 {
        let newest = [
            self.last_http_request_ms,
            self.last_exec_ms,
            if self.running_processes > 0 { now } else { 0 },
            if self.open_connections > 0 { now } else { 0 },
            if self.pending_jobs > 0 { now } else { 0 },
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        now.saturating_sub(newest)
    }
}

/// Where the signals come from (Docker stats, the jobs seam, the proxy...).
#[async_trait]
pub trait IdleProbe: Send + Sync {
    async fn sample(&self, sandbox_id: &str) -> SeamResult<IdleSignals>;
}

/// Applies the timeout policy to sampled signals.
pub struct IdleMonitor {
    probe: std::sync::Arc<dyn IdleProbe>,
    pub timeout_secs: u64,
    pub scan_secs: u64,
}

impl IdleMonitor {
    pub fn new(probe: std::sync::Arc<dyn IdleProbe>, timeout_secs: u64, scan_secs: u64) -> Self {
        Self {
            probe,
            timeout_secs,
            scan_secs,
        }
    }

    pub fn enabled(&self) -> bool {
        self.timeout_secs > 0 && self.scan_secs > 0
    }

    pub fn verdict(&self, signals: &IdleSignals, now: u64) -> IdleVerdict {
        if !self.enabled() {
            return IdleVerdict::Active {
                reason: "idle suspension disabled".to_string(),
            };
        }
        let timeout_ms = self.timeout_secs.saturating_mul(1000);
        match signals.active_reason(now, timeout_ms) {
            Some(reason) => IdleVerdict::Active { reason },
            None => IdleVerdict::Idle {
                idle_for_ms: signals.quiet_for_ms(now),
            },
        }
    }

    pub async fn scan(&self, sandbox_id: &str) -> SeamResult<IdleVerdict> {
        let signals = self.probe.sample(sandbox_id).await?;
        Ok(self.verdict(&signals, now_ms()))
    }
}

/// A probe for deployments without live metrics: everything reports idle, so
/// callers must opt in deliberately.
#[derive(Debug, Clone, Copy, Default)]
pub struct StaticProbe(pub IdleSignals);

#[async_trait]
impl IdleProbe for StaticProbe {
    async fn sample(&self, _sandbox_id: &str) -> SeamResult<IdleSignals> {
        Ok(self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: u64 = 30_000;

    fn idle_signals() -> IdleSignals {
        IdleSignals::default()
    }

    #[test]
    fn every_signal_keeps_the_sandbox_alive() {
        let now = 1_000_000;
        let cases = [
            IdleSignals {
                running_processes: 1,
                ..idle_signals()
            },
            IdleSignals {
                open_connections: 2,
                ..idle_signals()
            },
            IdleSignals {
                pending_jobs: 3,
                ..idle_signals()
            },
            IdleSignals {
                last_exec_ms: now - 1_000,
                ..idle_signals()
            },
            IdleSignals {
                last_http_request_ms: now - 1_000,
                ..idle_signals()
            },
        ];
        for (index, signals) in cases.iter().enumerate() {
            let reason = signals.active_reason(now, TIMEOUT);
            assert!(reason.is_some(), "signal {index} must count as activity");
        }
        // Process signal wins the explanation, so the log names the strongest one.
        assert!(cases[0]
            .active_reason(now, TIMEOUT)
            .unwrap()
            .contains("running process"));
    }

    #[test]
    fn stale_signals_do_not_count() {
        let now = 1_000_000;
        let signals = IdleSignals {
            last_exec_ms: now - TIMEOUT,
            last_http_request_ms: now - TIMEOUT - 5,
            ..idle_signals()
        };
        assert!(signals.active_reason(now, TIMEOUT).is_none());
        assert_eq!(signals.quiet_for_ms(now), TIMEOUT);
    }

    #[test]
    fn zero_timestamps_are_not_activity() {
        let signals = IdleSignals {
            last_exec_ms: 0,
            last_http_request_ms: 0,
            ..idle_signals()
        };
        assert!(signals.active_reason(10, TIMEOUT).is_none());
        assert_eq!(signals.quiet_for_ms(10), 10);
    }

    #[tokio::test]
    async fn monitor_policy_matches_the_signals() {
        let monitor = IdleMonitor::new(
            std::sync::Arc::new(StaticProbe(IdleSignals::default())),
            30,
            60,
        );
        assert!(monitor.enabled());
        let verdict = monitor.scan("sbx").await.unwrap();
        assert!(verdict.is_idle());

        let busy = IdleMonitor::new(
            std::sync::Arc::new(StaticProbe(IdleSignals {
                pending_jobs: 1,
                ..IdleSignals::default()
            })),
            30,
            60,
        );
        match busy.scan("sbx").await.unwrap() {
            IdleVerdict::Active { reason } => assert!(reason.contains("pending job")),
            other => panic!("expected active, got {other:?}"),
        }
    }

    #[test]
    fn disabled_monitor_never_suspends() {
        let monitor = IdleMonitor::new(
            std::sync::Arc::new(StaticProbe(IdleSignals::default())),
            0,
            0,
        );
        assert!(!monitor.enabled());
        let verdict = monitor.verdict(&IdleSignals::default(), now_ms());
        assert!(!verdict.is_idle());
    }

    #[test]
    fn verdict_reports_how_long_it_has_been_quiet() {
        let monitor = IdleMonitor::new(
            std::sync::Arc::new(StaticProbe(IdleSignals::default())),
            30,
            60,
        );
        let signals = IdleSignals {
            last_http_request_ms: 1_000,
            ..IdleSignals::default()
        };
        assert_eq!(
            monitor.verdict(&signals, 61_000),
            IdleVerdict::Idle {
                idle_for_ms: 60_000
            }
        );
    }
}
