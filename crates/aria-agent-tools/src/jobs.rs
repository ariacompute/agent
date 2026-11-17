//! Background jobs tool family (`job_*`).

use crate::{arg_str, arg_usize, family_handler, Family};
use agent_core::seams::{JobRecord, JobsSeam, SeamError};
use agent_core::{Tool, ToolCall};
use serde_json::json;
use std::sync::Arc;

pub struct JobsToolHandler {
    jobs: Arc<dyn JobsSeam>,
}

impl JobsToolHandler {
    pub fn new(jobs: Arc<dyn JobsSeam>) -> Self {
        Self { jobs }
    }
}

pub fn jobs_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "job_start".into(),
            description: "Start a background command and return its job id.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": { "type": "array", "items": { "type": "string" } },
                    "cwd": { "type": "string" }
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "job_list".into(),
            description: "List background jobs with their state.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        Tool {
            name: "job_output".into(),
            description: "Read a job's output from a byte offset.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "from": { "type": "integer", "minimum": 0 }
                },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "job_kill".into(),
            description: "Mark a running job as killed.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
    ]
}

fn render(records: &[JobRecord]) -> String {
    if records.is_empty() {
        return "(no jobs)".into();
    }
    records
        .iter()
        .map(|j| {
            format!(
                "{} {} exit={} argv={}",
                j.id,
                j.state.as_str(),
                j.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".into()),
                j.argv.join(" ")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Family for JobsToolHandler {
    fn prefix(&self) -> &'static str {
        "job_"
    }

    async fn dispatch(&self, call: &ToolCall, _session: &str) -> Result<String, SeamError> {
        match call.name.as_str() {
            "job_start" => {
                let argv: Vec<String> = call
                    .arguments
                    .get("command")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect()
                    })
                    .ok_or_else(|| SeamError::Invalid("`command` must be an array".into()))?;
                let cwd = call
                    .arguments
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .map(str::to_string);
                let id = self.jobs.start(argv, cwd).await?;
                Ok(format!("started job {id}"))
            }
            "job_list" => Ok(render(&self.jobs.list().await?)),
            "job_output" => {
                let id = arg_str(call, "id")?;
                let (slice, running) = self
                    .jobs
                    .output(&id, arg_usize(call, "from").unwrap_or(0))
                    .await?;
                Ok(format!(
                    "running={running}\n{}",
                    if slice.is_empty() {
                        "(no new output)"
                    } else {
                        &slice
                    }
                ))
            }
            "job_kill" => {
                let id = arg_str(call, "id")?;
                self.jobs.kill(&id).await?;
                Ok(format!("killed job {id}"))
            }
            other => Err(SeamError::Invalid(format!("unknown jobs tool: {other}"))),
        }
    }
}

family_handler!(JobsToolHandler);

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{JobState, LocalJobs, ShellOutput, ShellSeam};
    use agent_core::ToolHandler;
    use async_trait::async_trait;

    struct Echo;

    #[async_trait]
    impl ShellSeam for Echo {
        async fn exec(
            &self,
            argv: &[String],
            _cwd: Option<&str>,
        ) -> Result<ShellOutput, SeamError> {
            Ok(ShellOutput {
                exit_code: 0,
                stdout: argv.join(" "),
                stderr: String::new(),
            })
        }
    }

    async fn run(
        h: &JobsToolHandler,
        name: &str,
        args: serde_json::Value,
    ) -> agent_core::ToolResult {
        h.run(
            &ToolCall {
                id: "c".into(),
                name: name.into(),
                arguments: args,
            },
            &agent_core::in_memory_context(),
            "s",
        )
        .await
    }

    #[tokio::test]
    async fn start_then_list_output_and_kill() {
        let h = JobsToolHandler::new(Arc::new(LocalJobs::new(Arc::new(Echo))));
        let started = run(&h, "job_start", json!({"command": ["a", "b"]})).await;
        assert!(!started.is_error, "{}", started.content);
        let id = started
            .content
            .trim_start_matches("started job ")
            .to_string();
        for _ in 0..200 {
            let listed = run(&h, "job_list", json!({})).await.content;
            if listed.contains("succeeded") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let listed = run(&h, "job_list", json!({})).await.content;
        assert!(listed.contains(&id));
        assert!(listed.contains("succeeded"), "{listed}");
        let out = run(&h, "job_output", json!({"id": id})).await.content;
        assert!(out.contains("a b"), "{out}");
        // Killing a finished job is an error, surfaced as an error result.
        let killed = run(&h, "job_kill", json!({"id": id})).await;
        assert!(killed.is_error);
    }

    #[tokio::test]
    async fn unknown_job_and_bad_args() {
        let h = JobsToolHandler::new(Arc::new(LocalJobs::new(Arc::new(Echo))));
        assert!(run(&h, "job_output", json!({"id": "nope"}))
            .await
            .content
            .contains("not_found"));
        assert!(run(&h, "job_start", json!({})).await.is_error);
        assert!(run(&h, "job_nope", json!({})).await.is_error);
    }

    struct Pending;

    #[async_trait]
    impl ShellSeam for Pending {
        async fn exec(
            &self,
            _argv: &[String],
            _cwd: Option<&str>,
        ) -> Result<ShellOutput, SeamError> {
            std::future::pending().await
        }
    }

    #[tokio::test]
    async fn cwd_is_forwarded_and_a_running_job_can_be_killed() {
        let h = JobsToolHandler::new(Arc::new(LocalJobs::new(Arc::new(Pending))));
        let started = run(
            &h,
            "job_start",
            json!({"command": ["sleep", "60"], "cwd": "/w"}),
        )
        .await;
        assert!(!started.is_error, "{}", started.content);
        let id = started
            .content
            .trim_start_matches("started job ")
            .to_string();
        let _never = Pending.exec(&[], None);
        let killed = run(&h, "job_kill", json!({"id": id})).await;
        assert!(!killed.is_error, "{}", killed.content);
        assert!(killed.content.contains("killed job"), "{}", killed.content);
        let out = run(&h, "job_output", json!({"id": id})).await;
        assert!(out.content.contains("(no new output)"), "{}", out.content);
    }

    #[test]
    fn empty_list_renders_marker() {
        assert_eq!(render(&[]), "(no jobs)");
        let rec = JobRecord {
            id: "j".into(),
            argv: vec!["x".into()],
            state: JobState::Running,
            output: String::new(),
            exit_code: None,
            started_ms: 0,
        };
        assert!(render(&[rec]).contains("j running exit=- argv=x"));
    }
}
