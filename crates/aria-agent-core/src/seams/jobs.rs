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
            if let Some(job) = guard.get_mut(&task_id) {
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
            let start = from.min(bytes.len());
            // Slice on a char boundary so the result stays valid UTF-8.
            let mut end = bytes.len();
            while end > start && !job.output.is_char_boundary(end) {
                end -= 1;
            }
            let mut start = start;
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

    #[test]
    fn output_is_bounded_on_char_boundaries() {
        let big = "é".repeat(MAX_JOB_OUTPUT_BYTES);
        let bounded = bound(big);
        assert!(bounded.len() <= MAX_JOB_OUTPUT_BYTES + 32);
        assert!(bounded.ends_with("[output truncated]"));
        assert!(std::str::from_utf8(bounded.as_bytes()).is_ok());
    }
}
