//! `aria-agent-orchestrator` — sub-agents, agent teams and scheduled jobs.
//!
//! Three independent providers, each registered on its own seam. Nothing is
//! installed by default: a deployment that does not configure a provider simply
//! does not see the matching tools ([`Orchestrator::tools`] only offers what can
//! actually be served), and a handler that is constructed manually reports
//! `ERROR [unavailable]` instead of faking success (ADR-0017).

pub mod schedule;
pub mod subagent;
pub mod teams;

use agent_core::seams::{
    ScheduleEntry, ScheduleSeam, SeamError, SeamResult, SubagentRequest, Subagents,
};
use agent_core::{Tool, ToolCall, ToolHandler, ToolResult};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;
use teams::{AgentTeam, TeamStore, TeamTask};

pub use schedule::{now_ms, MemorySchedule, ScheduleRunner};
pub use subagent::{InProcessSubagents, SubagentFactory};
pub use teams::{MemoryTeams, TaskState, TeamTask as Task};

// ------------------------------------------------------------- tool schemas

/// `subagent_*` schemas.
pub fn subagent_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "subagent_run".into(),
            description: "Delegate a self-contained task to a child agent and return its answer."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "task": { "type": "string" },
                    "agent_name": { "type": "string" },
                    "max_steps": { "type": "integer", "minimum": 0 }
                },
                "required": ["task"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "subagent_list".into(),
            description: "List the child agents of this session.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
    ]
}

/// `team_*` schemas.
pub fn team_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "team_create".into(),
            description: "Create a task DAG that several agents can work through.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "tasks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string" },
                                "title": { "type": "string" },
                                "depends_on": {
                                    "type": "array",
                                    "items": { "type": "string" }
                                }
                            },
                            "required": ["title"],
                            "additionalProperties": false
                        }
                    }
                },
                "required": ["name", "tasks"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "team_status".into(),
            description: "Show a team's tasks, states and ready work.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "team": { "type": "string" } },
                "required": ["team"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "team_claim".into(),
            description: "Claim a ready task of a team.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "team": { "type": "string" },
                    "task": { "type": "string" },
                    "worker": { "type": "string" }
                },
                "required": ["team", "task"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "team_complete".into(),
            description: "Complete a claimed task and record its result.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "team": { "type": "string" },
                    "task": { "type": "string" },
                    "result": { "type": "string" }
                },
                "required": ["team", "task", "result"],
                "additionalProperties": false
            }),
        },
    ]
}

/// `schedule_*` schemas.
pub fn schedule_tools() -> Vec<Tool> {
    vec![
        Tool {
            name: "schedule_add".into(),
            description: "Schedule a recurring (`every_secs`) or one-shot (`at_ms`) job.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string" },
                    "every_secs": { "type": "integer", "minimum": 1 },
                    "at_ms": { "type": "integer", "minimum": 1 },
                    "payload": {}
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        },
        Tool {
            name: "schedule_list".into(),
            description: "List the scheduled jobs of this tenant.".into(),
            parameters: json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
        },
        Tool {
            name: "schedule_remove".into(),
            description: "Delete a scheduled job.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        },
    ]
}

/// Every orchestrator schema, regardless of what the deployment configured.
pub fn orchestrator_tools() -> Vec<Tool> {
    let mut tools = subagent_tools();
    tools.extend(team_tools());
    tools.extend(schedule_tools());
    tools
}

// ------------------------------------------------------------------ helpers

fn arg_str(call: &ToolCall, key: &str) -> SeamResult<String> {
    call.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            SeamError::Invalid(format!(
                "tool `{}` requires string argument `{key}`",
                call.name
            ))
        })
}

fn arg_str_opt(call: &ToolCall, key: &str) -> Option<String> {
    call.arguments
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn arg_u64(call: &ToolCall, key: &str) -> Option<u64> {
    call.arguments.get(key).and_then(Value::as_u64)
}

fn ok(call: &ToolCall, content: impl Into<String>) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content: content.into(),
        is_error: false,
    }
}

fn fail(call: &ToolCall, err: SeamError) -> ToolResult {
    ToolResult {
        call_id: call.id.clone(),
        content: format!("ERROR [{}]: {err}", err.code()),
        is_error: true,
    }
}

fn parse_tasks(call: &ToolCall) -> SeamResult<Vec<TeamTask>> {
    let raw = call
        .arguments
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| SeamError::Invalid("`tasks` must be an array".into()))?;
    let mut tasks = Vec::with_capacity(raw.len());
    for (index, item) in raw.iter().enumerate() {
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let id = item
            .get("id")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("t{}", index + 1));
        let deps: Vec<String> = item
            .get("depends_on")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|d| d.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        tasks.push(TeamTask::new(id, title).after(deps));
    }
    Ok(tasks)
}

fn render_schedule(entry: &ScheduleEntry) -> String {
    let trigger = match (entry.every_secs, entry.at_ms) {
        (Some(secs), _) => format!("every {secs}s"),
        (None, Some(at)) => format!("at {at}"),
        _ => "unscheduled".to_string(),
    };
    let next = entry
        .next_run_ms
        .map(|n| n.to_string())
        .unwrap_or_else(|| "-".into());
    format!(
        "{} `{}` {}{} next={next}",
        entry.id,
        entry.name,
        trigger,
        if entry.enabled { "" } else { " (disabled)" }
    )
}

// ---------------------------------------------------------------- providers

/// Subagent tools, backed by a [`Subagents`] provider.
pub struct SubagentTools {
    subagents: Arc<dyn Subagents>,
}

impl SubagentTools {
    pub fn new(subagents: Arc<dyn Subagents>) -> Self {
        Self { subagents }
    }
}

#[async_trait]
impl ToolHandler for SubagentTools {
    fn handles(&self, name: &str) -> bool {
        name.starts_with("subagent_")
    }

    async fn run(
        &self,
        call: &ToolCall,
        _ctx: &Arc<dyn agent_core::context::ContextStore>,
        session: &str,
    ) -> ToolResult {
        let outcome = async {
            match call.name.as_str() {
                "subagent_run" => {
                    let task = arg_str(call, "task")?;
                    let mut req = SubagentRequest::new(session, task);
                    req.agent_name = arg_str_opt(call, "agent_name");
                    req.max_steps = arg_u64(call, "max_steps").unwrap_or(0) as usize;
                    let handle = self.subagents.spawn(req).await?;
                    let answer = self.subagents.wait(&handle.id).await?;
                    Ok::<String, SeamError>(format!(
                        "subagent {} finished (session {})\n\n{answer}",
                        handle.id, handle.session
                    ))
                }
                "subagent_list" => {
                    let children = self.subagents.list(session).await?;
                    if children.is_empty() {
                        return Ok("(no subagents)".to_string());
                    }
                    Ok(children
                        .into_iter()
                        .map(|c| format!("{} -> {}", c.id, c.session))
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
                other => Err(SeamError::Invalid(format!("unknown tool: {other}"))),
            }
        }
        .await;
        match outcome {
            Ok(content) => ok(call, content),
            Err(e) => fail(call, e),
        }
    }
}

/// Team tools, backed by a [`TeamStore`].
pub struct TeamTools {
    teams: Arc<dyn TeamStore>,
    /// Tenant override; defaults to the session id.
    owner: Option<String>,
}

impl TeamTools {
    pub fn new(teams: Arc<dyn TeamStore>) -> Self {
        Self { teams, owner: None }
    }

    /// Pin the tenant (the cloud runtime does this from the resolved principal).
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    fn owner(&self, session: &str) -> String {
        self.owner.clone().unwrap_or_else(|| session.to_string())
    }
}

#[async_trait]
impl ToolHandler for TeamTools {
    fn handles(&self, name: &str) -> bool {
        name.starts_with("team_")
    }

    async fn run(
        &self,
        call: &ToolCall,
        _ctx: &Arc<dyn agent_core::context::ContextStore>,
        session: &str,
    ) -> ToolResult {
        let owner = self.owner(session);
        let outcome = async {
            match call.name.as_str() {
                "team_create" => {
                    let name = arg_str(call, "name")?;
                    let tasks = parse_tasks(call)?;
                    let team = AgentTeam::new(owner.clone(), name, tasks)?;
                    let stored = self.teams.create(team).await?;
                    Ok::<String, SeamError>(format!("{}\nteam id: {}", stored.render(), stored.id))
                }
                "team_status" => {
                    let id = arg_str(call, "team")?;
                    let team = self.teams.get(&owner, &id).await?;
                    Ok(team.render())
                }
                "team_claim" => {
                    let id = arg_str(call, "team")?;
                    let task = arg_str(call, "task")?;
                    let worker = arg_str_opt(call, "worker").unwrap_or_else(|| session.to_string());
                    let mut team = self.teams.get(&owner, &id).await?;
                    team.claim(&task, &worker)?;
                    let updated = self.teams.update(team).await?;
                    Ok(format!(
                        "claimed `{task}` for {worker}\n{}",
                        updated.render()
                    ))
                }
                "team_complete" => {
                    let id = arg_str(call, "team")?;
                    let task = arg_str(call, "task")?;
                    let result = arg_str(call, "result")?;
                    let mut team = self.teams.get(&owner, &id).await?;
                    team.complete(&task, result)?;
                    let updated = self.teams.update(team).await?;
                    Ok(format!("completed `{task}`\n{}", updated.render()))
                }
                other => Err(SeamError::Invalid(format!("unknown tool: {other}"))),
            }
        }
        .await;
        match outcome {
            Ok(content) => ok(call, content),
            Err(e) => fail(call, e),
        }
    }
}

/// Schedule tools, backed by a [`ScheduleSeam`].
pub struct ScheduleTools {
    schedule: Arc<dyn ScheduleSeam>,
    owner: Option<String>,
}

impl ScheduleTools {
    pub fn new(schedule: Arc<dyn ScheduleSeam>) -> Self {
        Self {
            schedule,
            owner: None,
        }
    }

    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    fn owner(&self, session: &str) -> String {
        self.owner.clone().unwrap_or_else(|| session.to_string())
    }
}

#[async_trait]
impl ToolHandler for ScheduleTools {
    fn handles(&self, name: &str) -> bool {
        name.starts_with("schedule_")
    }

    async fn run(
        &self,
        call: &ToolCall,
        _ctx: &Arc<dyn agent_core::context::ContextStore>,
        session: &str,
    ) -> ToolResult {
        let owner = self.owner(session);
        let outcome = async {
            match call.name.as_str() {
                "schedule_add" => {
                    let name = arg_str(call, "name")?;
                    let every = arg_u64(call, "every_secs");
                    let at = arg_u64(call, "at_ms").map(|v| v as i64);
                    let mut entry = match (every, at) {
                        (Some(secs), None) => ScheduleEntry::every(owner.clone(), name, secs),
                        (None, Some(ms)) => ScheduleEntry::at(owner.clone(), name, ms),
                        _ => {
                            return Err(SeamError::Invalid(
                                "exactly one of `every_secs` / `at_ms` is required".into(),
                            ))
                        }
                    };
                    entry.payload = call
                        .arguments
                        .get("payload")
                        .cloned()
                        .unwrap_or(Value::Null);
                    let stored = self.schedule.upsert(entry).await?;
                    Ok::<String, SeamError>(format!("scheduled {}", render_schedule(&stored)))
                }
                "schedule_list" => {
                    let entries = self.schedule.list(&owner).await?;
                    if entries.is_empty() {
                        return Ok("(no scheduled jobs)".to_string());
                    }
                    Ok(entries
                        .iter()
                        .map(render_schedule)
                        .collect::<Vec<_>>()
                        .join("\n"))
                }
                "schedule_remove" => {
                    let id = arg_str(call, "id")?;
                    if self.schedule.remove(&owner, &id).await? {
                        Ok(format!("removed `{id}`"))
                    } else {
                        Ok(format!("no scheduled job `{id}`"))
                    }
                }
                other => Err(SeamError::Invalid(format!("unknown tool: {other}"))),
            }
        }
        .await;
        match outcome {
            Ok(content) => ok(call, content),
            Err(e) => fail(call, e),
        }
    }
}

// --------------------------------------------------------------- assembly

/// Which orchestrator families the deployment can serve.
#[derive(Default, Clone)]
pub struct Orchestrator {
    pub subagents: Option<Arc<dyn Subagents>>,
    pub teams: Option<Arc<dyn TeamStore>>,
    pub schedule: Option<Arc<dyn ScheduleSeam>>,
    /// Tenant id used for teams/schedule (defaults to the session id).
    pub owner: Option<String>,
}

impl Orchestrator {
    /// In-process everything: forked subagents, memory teams, memory schedule.
    pub fn in_process(factory: Arc<dyn subagent::SubagentFactory>) -> Self {
        Self {
            subagents: Some(Arc::new(InProcessSubagents::new(factory))),
            teams: Some(Arc::new(teams::MemoryTeams::new())),
            schedule: Some(Arc::new(MemorySchedule::new())),
            owner: None,
        }
    }

    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = Some(owner.into());
        self
    }

    /// Schemas for the families that are actually configured.
    pub fn tools(&self) -> Vec<Tool> {
        let mut tools = Vec::new();
        if self.subagents.is_some() {
            tools.extend(subagent_tools());
        }
        if self.teams.is_some() {
            tools.extend(team_tools());
        }
        if self.schedule.is_some() {
            tools.extend(schedule_tools());
        }
        tools
    }

    pub fn handlers(&self) -> Vec<Arc<dyn ToolHandler>> {
        let mut handlers: Vec<Arc<dyn ToolHandler>> = Vec::new();
        if let Some(subagents) = &self.subagents {
            handlers.push(Arc::new(SubagentTools::new(subagents.clone())));
        }
        if let Some(teams) = &self.teams {
            let tools = TeamTools::new(teams.clone());
            handlers.push(Arc::new(match &self.owner {
                Some(owner) => tools.with_owner(owner.clone()),
                None => tools,
            }));
        }
        if let Some(schedule) = &self.schedule {
            let tools = ScheduleTools::new(schedule.clone());
            handlers.push(Arc::new(match &self.owner {
                Some(owner) => tools.with_owner(owner.clone()),
                None => tools,
            }));
        }
        handlers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::SubagentHandle;
    use agent_core::ToolResult as CoreToolResult;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// Provider that records requests and answers deterministically.
    struct FixedSubagents {
        seen: Mutex<Vec<SubagentRequest>>,
        calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl Subagents for FixedSubagents {
        async fn spawn(&self, req: SubagentRequest) -> SeamResult<SubagentHandle> {
            self.seen.lock().unwrap().push(req.clone());
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(SubagentHandle {
                id: "child-1".into(),
                session: format!("{}::sub::child-1", req.parent_session),
                parent_session: req.parent_session,
            })
        }

        async fn list(&self, parent_session: &str) -> SeamResult<Vec<SubagentHandle>> {
            Ok(vec![SubagentHandle {
                id: "child-1".into(),
                session: format!("{parent_session}::sub::child-1"),
                parent_session: parent_session.to_string(),
            }])
        }

        async fn wait(&self, _id: &str) -> SeamResult<String> {
            Ok("child answer".into())
        }
    }

    fn ctx() -> Arc<dyn agent_core::context::ContextStore> {
        agent_core::in_memory_context()
    }

    async fn run(handler: &Arc<dyn ToolHandler>, name: &str, args: Value) -> CoreToolResult {
        handler
            .run(
                &ToolCall {
                    id: "c".into(),
                    name: name.into(),
                    arguments: args,
                },
                &ctx(),
                "sess-1",
            )
            .await
    }

    #[tokio::test]
    async fn subagent_run_returns_the_child_answer_and_records_the_request() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FixedSubagents {
            seen: Mutex::new(Vec::new()),
            calls: calls.clone(),
        });
        let handler: Arc<dyn ToolHandler> = Arc::new(SubagentTools::new(provider.clone()));
        let res = run(&handler, "subagent_run", json!({"task": "look it up"})).await;
        assert!(!res.is_error, "{}", res.content);
        assert!(res.content.contains("child answer"));
        assert!(res.content.contains("sess-1::sub::child-1"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let seen = provider.seen.lock().unwrap();
        assert_eq!(seen[0].task, "look it up");
        assert_eq!(seen[0].parent_session, "sess-1");
    }

    #[tokio::test]
    async fn subagent_tools_fail_closed_without_a_provider_and_validate_args() {
        let calls = Arc::new(AtomicUsize::new(0));
        let provider = Arc::new(FixedSubagents {
            seen: Mutex::new(Vec::new()),
            calls,
        });
        let handler: Arc<dyn ToolHandler> = Arc::new(SubagentTools::new(provider));

        let listed = run(&handler, "subagent_list", json!({})).await;
        assert!(listed.content.contains("child-1"));

        let missing = run(&handler, "subagent_run", json!({})).await;
        assert!(missing.is_error);
        assert!(missing.content.contains("invalid"));

        let unknown = run(&handler, "subagent_nope", json!({})).await;
        assert!(unknown.is_error);

        let noop: Arc<dyn Subagents> = Arc::new(agent_core::seams::NoSubagents);
        let handler: Arc<dyn ToolHandler> = Arc::new(SubagentTools::new(noop));
        let denied = run(&handler, "subagent_run", json!({"task": "x"})).await;
        assert!(denied.is_error);
        assert!(denied.content.contains("unavailable"), "{}", denied.content);

        struct ListsNothing;
        #[async_trait]
        impl Subagents for ListsNothing {
            async fn spawn(&self, _r: SubagentRequest) -> SeamResult<SubagentHandle> {
                Err(SeamError::Unavailable("none".into()))
            }
            async fn list(&self, _p: &str) -> SeamResult<Vec<SubagentHandle>> {
                Ok(Vec::new())
            }
        }
        let handler: Arc<dyn ToolHandler> = Arc::new(SubagentTools::new(Arc::new(ListsNothing)));
        assert_eq!(
            run(&handler, "subagent_list", json!({})).await.content,
            "(no subagents)"
        );
        assert!(
            run(&handler, "subagent_run", json!({"task": "x"}))
                .await
                .is_error
        );
    }

    #[tokio::test]
    async fn team_tools_create_claim_and_complete_a_dag() {
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeams::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(TeamTools::new(store));

        let created = run(
            &handler,
            "team_create",
            json!({
                "name": "release",
                "tasks": [
                    {"title": "spec"},
                    {"title": "impl", "depends_on": ["t1"]}
                ]
            }),
        )
        .await;
        assert!(!created.is_error, "{}", created.content);
        assert!(
            created.content.contains("team id: team-1"),
            "{}",
            created.content
        );
        assert!(created.content.contains("0/2 done"));

        let blocked = run(
            &handler,
            "team_claim",
            json!({"team": "team-1", "task": "t2"}),
        )
        .await;
        assert!(blocked.is_error, "dependency must gate the claim");
        assert!(blocked.content.contains("blocked"), "{}", blocked.content);

        let claimed = run(
            &handler,
            "team_claim",
            json!({"team": "team-1", "task": "t1", "worker": "w1"}),
        )
        .await;
        assert!(!claimed.is_error, "{}", claimed.content);
        assert!(claimed.content.contains("claimed `t1` for w1"));

        let done = run(
            &handler,
            "team_complete",
            json!({"team": "team-1", "task": "t1", "result": "spec ok"}),
        )
        .await;
        assert!(!done.is_error, "{}", done.content);
        assert!(done.content.contains("1/2 done"));

        let status = run(&handler, "team_status", json!({"team": "team-1"})).await;
        assert!(status.content.contains("[done] spec"));
        assert!(status.content.contains("[pending] impl"));

        let unknown = run(&handler, "team_status", json!({"team": "nope"})).await;
        assert!(unknown.is_error);
        assert!(unknown.content.contains("not_found"));

        let bad = run(&handler, "team_create", json!({"name": "x"})).await;
        assert!(bad.is_error);
        assert!(bad.content.contains("`tasks` must be an array"));

        assert!(run(&handler, "team_nope", json!({})).await.is_error);
    }

    #[tokio::test]
    async fn team_tools_are_tenant_scoped() {
        let store: Arc<dyn TeamStore> = Arc::new(MemoryTeams::new());
        let handler: Arc<dyn ToolHandler> =
            Arc::new(TeamTools::new(store.clone()).with_owner("tenant-a"));
        let created = run(
            &handler,
            "team_create",
            json!({"name": "t", "tasks": [{"title": "only"}]}),
        )
        .await;
        assert!(!created.is_error);

        // Same store, different tenant: the id exists but is invisible.
        let other: Arc<dyn ToolHandler> = Arc::new(TeamTools::new(store).with_owner("tenant-b"));
        let peek = run(&other, "team_status", json!({"team": "team-1"})).await;
        assert!(peek.is_error);
        assert!(peek.content.contains("not_found"), "{}", peek.content);
    }

    #[tokio::test]
    async fn schedule_tools_add_list_and_remove() {
        let store: Arc<dyn ScheduleSeam> = Arc::new(MemorySchedule::new());
        let handler: Arc<dyn ToolHandler> = Arc::new(ScheduleTools::new(store));

        let added = run(
            &handler,
            "schedule_add",
            json!({"name": "daily", "every_secs": 60, "payload": {"prompt": "hi"}}),
        )
        .await;
        assert!(!added.is_error, "{}", added.content);
        assert!(added.content.contains("every 60s"), "{}", added.content);

        let at = run(
            &handler,
            "schedule_add",
            json!({"name": "once", "at_ms": 9_000_000_000i64}),
        )
        .await;
        assert!(at.content.contains("at 9000000000"), "{}", at.content);

        let listed = run(&handler, "schedule_list", json!({})).await;
        assert!(listed.content.contains("daily"));

        let bad = run(&handler, "schedule_add", json!({"name": "x"})).await;
        assert!(bad.is_error);
        assert!(bad.content.contains("exactly one"));
        let both = run(
            &handler,
            "schedule_add",
            json!({"name": "x", "every_secs": 5, "at_ms": 10}),
        )
        .await;
        assert!(both.is_error);

        assert!(run(&handler, "schedule_nope", json!({})).await.is_error);
        let removed = run(&handler, "schedule_remove", json!({"id": "job-1"})).await;
        assert!(removed.content.contains("removed"));
        let again = run(&handler, "schedule_remove", json!({"id": "job-1"})).await;
        assert!(again.content.contains("no scheduled job"));
        let removed_at = run(&handler, "schedule_remove", json!({"id": "job-2"})).await;
        assert!(removed_at.content.contains("removed"));
        assert_eq!(
            run(&handler, "schedule_list", json!({})).await.content,
            "(no scheduled jobs)"
        );
    }

    #[test]
    fn schedule_rendering_covers_every_trigger() {
        let mut at = ScheduleEntry::at("t", "n", 1000);
        at.next_run_ms = None;
        let rendered = render_schedule(&at);
        assert!(rendered.contains("at 1000"), "{rendered}");
        assert!(rendered.contains("next=-"), "{rendered}");
        let mut bare = ScheduleEntry::every("t", "n", 1);
        bare.every_secs = None;
        bare.at_ms = None;
        assert!(render_schedule(&bare).contains("unscheduled"));
    }

    #[tokio::test]
    async fn an_empty_orchestrator_offers_nothing() {
        let orch = Orchestrator::default();
        assert!(orch.tools().is_empty());
        assert!(orch.handlers().is_empty());
        assert!(
            !orchestrator_tools().is_empty(),
            "schemas stay introspectable"
        );
    }

    #[tokio::test]
    async fn orchestrator_wires_only_configured_families() {
        struct Echo;
        #[async_trait]
        impl SubagentFactory for Echo {
            async fn run(&self, req: &SubagentRequest, session: &str) -> SeamResult<String> {
                Ok(format!("{session}: {}", req.task))
            }
        }
        let orch = Orchestrator::in_process(Arc::new(Echo)).with_owner("tenant-a");
        let names: Vec<String> = orch.tools().into_iter().map(|t| t.name).collect();
        for want in ["subagent_run", "team_create", "schedule_add"] {
            assert!(
                names.iter().any(|n| n == want),
                "missing {want} in {names:?}"
            );
        }
        assert_eq!(orch.handlers().len(), 3);

        let ownerless = Orchestrator::in_process(Arc::new(Echo));
        assert_eq!(ownerless.handlers().len(), 3);

        let subagents = orch.subagents.clone().unwrap();
        let handle = subagents
            .spawn(SubagentRequest::new("p", "task"))
            .await
            .unwrap();
        assert_eq!(
            subagents.wait(&handle.id).await.unwrap(),
            format!("{}: task", handle.session)
        );
    }
}
