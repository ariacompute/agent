//! Background jobs seam.
//!
//! `tool-bash` / `tool-terminal` / `tool-jobs` all consume one producer registry
//! (dsh `ctx.jobs`). The contract is deliberately small: start, list, read a
//! slice of output, kill. Output is bounded by [`MAX_JOB_OUTPUT_BYTES`] so a
//! runaway process cannot exhaust memory.

use super::{SeamError, SeamResult, ShellSeam};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// Identifier of a background job.
pub type JobId = String;

/// Upper bound of retained output per job.
pub const MAX_JOB_OUTPUT_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Running,
    Succeeded,
    Failed,
    Killed,
}

impl JobState {
    pub fn is_terminal(&self) -> bool {
        !matches!(self, JobState::Running)
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            JobState::Running => "running",
            JobState::Succeeded => "succeeded",
            JobState::Failed => "failed",
            JobState::Killed => "killed",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRecord {
    pub id: JobId,
    pub argv: Vec<String>,
    pub state: JobState,
    pub output: String,
    pub exit_code: Option<i64>,
    pub started_ms: i64,
}

#[async_trait]
pub trait JobsSeam: Send + Sync {
    async fn start(&self, argv: Vec<String>, cwd: Option<String>) -> SeamResult<JobId>;
    async fn list(&self) -> SeamResult<Vec<JobRecord>>;
    /// Output from byte offset `from`; the boolean reports whether the job is
    /// still running.
    async fn output(&self, id: &str, from: usize) -> SeamResult<(String, bool)>;
    async fn kill(&self, id: &str) -> SeamResult<()>;
}

/// In-process job registry backed by a [`ShellSeam`].
pub struct LocalJobs {
    shell: Arc<dyn ShellSeam>,
    jobs: Arc<Mutex<BTreeMap<JobId, JobRecord>>>,
}

impl LocalJobs {
    pub fn new(shell: Arc<dyn ShellSeam>) -> Self {
        Self {
            shell,
            jobs: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn with_job<T>(
        &self,
        id: &str,
        f: impl FnOnce(&mut JobRecord) -> SeamResult<T>,
    ) -> SeamResult<T> {
        let mut guard = self.jobs.lock().unwrap();
        let job = guard
            .get_mut(id)
            .ok_or_else(|| SeamError::NotFound(format!("job {id}")))?;
        f(job)
    }
}

#[async_trait]
impl JobsSeam for LocalJobs {
    async fn start(&self, argv: Vec<String>, cwd: Option<String>) -> SeamResult<JobId> {
        if argv.is_empty() {
            return Err(SeamError::Invalid("empty command".into()));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let record = JobRecord {
            id: id.clone(),
            argv: argv.clone(),
            state: JobState::Running,
            output: String::new(),
            exit_code: None,
            started_ms: crate::context::now_ms(),
        };
        self.jobs.lock().unwrap().insert(id.clone(), record);

        let shell = self.shell.clone();
        let jobs = self.jobs.clone();
        let task_id = id.clone();
        tokio::spawn(async move {
            let result = shell.exec(&argv, cwd.as_deref()).await;
            let mut guard = jobs.lock().unwrap();
            // The record was inserted above and jobs are never removed.
            let job = guard.get_mut(&task_id).expect("job record exists");
            match result {
                Ok(out) => {
                    job.output = bound(format!("{}{}", out.stdout, out.stderr));
                    job.exit_code = Some(out.exit_code);
                    job.state = if job.state == JobState::Killed {
                        JobState::Killed
                    } else if out.success() {
                        JobState::Succeeded
                    } else {
                        JobState::Failed
                    };
                }
                Err(e) => {
                    job.output = bound(format!("error: {e}"));
                    job.state = if job.state == JobState::Killed {
                        JobState::Killed
                    } else {
                        JobState::Failed
                    };
                }
            }
        });
        Ok(id)
    }

    async fn list(&self) -> SeamResult<Vec<JobRecord>> {
        Ok(self.jobs.lock().unwrap().values().cloned().collect())
    }

    async fn output(&self, id: &str, from: usize) -> SeamResult<(String, bool)> {
        self.with_job(id, |job| {
            let bytes = job.output.as_bytes();
            // `end` is always a char boundary (String length); only `start`
            // from the caller can land inside a multi-byte codepoint.
            let end = bytes.len();
            let mut start = from.min(end);
            while start < end && !job.output.is_char_boundary(start) {
                start += 1;
            }
            Ok((job.output[start..end].to_string(), !job.state.is_terminal()))
        })
    }

    async fn kill(&self, id: &str) -> SeamResult<()> {
        self.with_job(id, |job| {
            if job.state.is_terminal() {
                return Err(SeamError::Invalid(format!(
                    "job {id} already finished ({})",
                    job.state.as_str()
                )));
            }
            job.state = JobState::Killed;
            Ok(())
        })
    }
}

fn bound(mut s: String) -> String {
    if s.len() <= MAX_JOB_OUTPUT_BYTES {
        return s;
    }
    let mut cut = MAX_JOB_OUTPUT_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s.truncate(cut);
    s.push_str("\n[output truncated]");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::seams::shell::ShellOutput;

    struct FixedShell {
        out: ShellOutput,
    }

    #[async_trait]
    impl ShellSeam for FixedShell {
        async fn exec(&self, _argv: &[String], _cwd: Option<&str>) -> SeamResult<ShellOutput> {
            Ok(self.out.clone())
        }
    }

    fn shell(code: i64, stdout: &str) -> Arc<dyn ShellSeam> {
        Arc::new(FixedShell {
            out: ShellOutput {
                exit_code: code,
                stdout: stdout.into(),
                stderr: String::new(),
            },
        })
    }

    async fn settle(jobs: &LocalJobs, id: &str) -> JobRecord {
        for _ in 0..200 {
            let rec = jobs
                .list()
                .await
                .unwrap()
                .into_iter()
                .find(|j| j.id == id)
                .unwrap();
            if rec.state.is_terminal() {
                return rec;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("job did not settle");
    }

    #[tokio::test]
    async fn start_records_success_and_output() {
        let jobs = LocalJobs::new(shell(0, "hello"));
        let id = jobs
            .start(vec!["echo".into(), "hi".into()], None)
            .await
            .unwrap();
        let rec = settle(&jobs, &id).await;
        assert_eq!(rec.state, JobState::Succeeded);
        assert_eq!(rec.exit_code, Some(0));
        assert_eq!(rec.output, "hello");
        let (slice, running) = jobs.output(&id, 1).await.unwrap();
        assert_eq!(slice, "ello");
        assert!(!running);
    }

    #[tokio::test]
    async fn non_zero_exit_marks_failed() {
        let jobs = LocalJobs::new(shell(2, ""));
        let id = jobs.start(vec!["false".into()], None).await.unwrap();
        assert_eq!(settle(&jobs, &id).await.state, JobState::Failed);
    }

    #[tokio::test]
    async fn empty_command_and_unknown_ids_are_rejected() {
        let jobs = LocalJobs::new(shell(0, ""));
        assert!(matches!(
            jobs.start(vec![], None).await,
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            jobs.output("nope", 0).await,
            Err(SeamError::NotFound(_))
        ));
        assert!(matches!(
            jobs.kill("nope").await,
            Err(SeamError::NotFound(_))
        ));
    }

    #[tokio::test]
    async fn kill_on_finished_job_is_an_error() {
        let jobs = LocalJobs::new(shell(0, ""));
        let id = jobs.start(vec!["ls".into()], None).await.unwrap();
        settle(&jobs, &id).await;
        assert!(matches!(jobs.kill(&id).await, Err(SeamError::Invalid(_))));
    }

    struct FailingShell;

    #[async_trait]
    impl ShellSeam for FailingShell {
        async fn exec(&self, _argv: &[String], _cwd: Option<&str>) -> SeamResult<ShellOutput> {
            Err(SeamError::Io("boom".into()))
        }
    }

    struct SlowShell;

    #[async_trait]
    impl ShellSeam for SlowShell {
        async fn exec(&self, _argv: &[String], _cwd: Option<&str>) -> SeamResult<ShellOutput> {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Ok(ShellOutput {
                exit_code: 0,
                stdout: "late".into(),
                stderr: String::new(),
            })
        }
    }

    struct PendingShell;

    #[async_trait]
    impl ShellSeam for PendingShell {
        async fn exec(&self, _argv: &[String], _cwd: Option<&str>) -> SeamResult<ShellOutput> {
            std::future::pending().await
        }
    }

    #[test]
    fn every_state_has_a_stable_label() {
        assert_eq!(JobState::Running.as_str(), "running");
        assert_eq!(JobState::Succeeded.as_str(), "succeeded");
        assert_eq!(JobState::Failed.as_str(), "failed");
        assert_eq!(JobState::Killed.as_str(), "killed");
        assert!(!JobState::Running.is_terminal());
        assert!(JobState::Killed.is_terminal());
    }

    #[tokio::test]
    async fn shell_errors_mark_the_job_failed() {
        let jobs = LocalJobs::new(Arc::new(FailingShell));
        let id = jobs.start(vec!["boom".into()], None).await.unwrap();
        let rec = settle(&jobs, &id).await;
        assert_eq!(rec.state, JobState::Failed);
        assert!(rec.output.contains("error: boom"), "{}", rec.output);
    }

    #[tokio::test]
    async fn killing_a_running_job_stays_killed_after_the_task_returns() {
        let jobs = LocalJobs::new(Arc::new(SlowShell));
        let id = jobs.start(vec!["sleep".into()], None).await.unwrap();
        jobs.kill(&id).await.unwrap();
        // Wait past the shell's delay so the spawned task processes the kill.
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let rec = settle(&jobs, &id).await;
        assert_eq!(rec.state, JobState::Killed);
    }

    struct SlowFailingShell;

    #[async_trait]
    impl ShellSeam for SlowFailingShell {
        async fn exec(&self, _argv: &[String], _cwd: Option<&str>) -> SeamResult<ShellOutput> {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            Err(SeamError::Io("late boom".into()))
        }
    }

    #[tokio::test]
    async fn killing_a_job_that_then_errors_stays_killed() {
        let jobs = LocalJobs::new(Arc::new(SlowFailingShell));
        let id = jobs.start(vec!["sleep".into()], None).await.unwrap();
        jobs.kill(&id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        let rec = settle(&jobs, &id).await;
        assert_eq!(rec.state, JobState::Killed);
    }

    #[tokio::test]
    async fn output_start_is_snapped_forward_to_a_char_boundary() {
        let jobs = LocalJobs::new(shell(0, "ééé"));
        let id = jobs.start(vec!["echo".into()], None).await.unwrap();
        settle(&jobs, &id).await;
        let (slice, _) = jobs.output(&id, 1).await.unwrap();
        assert_eq!(slice, "éé");
    }

    #[test]
    fn output_is_bounded_on_char_boundaries() {
        let big = "é".repeat(MAX_JOB_OUTPUT_BYTES);
        let bounded = bound(big);
        assert!(bounded.len() <= MAX_JOB_OUTPUT_BYTES + 32);
        assert!(bounded.ends_with("[output truncated]"));
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
        let three_byte = "€".repeat(MAX_JOB_OUTPUT_BYTES);
        let bounded = bound(three_byte);
        assert!(bounded.len() <= MAX_JOB_OUTPUT_BYTES + 32);
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }

    #[tokio::test]
    #[should_panic(expected = "job did not settle")]
    async fn settle_panics_when_a_job_never_finishes() {
        let jobs = LocalJobs::new(Arc::new(PendingShell));
        let id = jobs.start(vec!["pending".into()], None).await.unwrap();
        settle(&jobs, &id).await;
    }
}
