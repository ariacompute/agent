//! Agent teams: a task DAG several agents can share.
//!
//! A team is created once (with its dependency graph validated eagerly: unknown
//! ids, duplicate ids and cycles are refused *before* any task can be claimed),
//! then tasks move `pending → claimed → done`. Claiming is dependency-gated, so
//! two workers can never start the same task and no worker can jump the DAG
//! (ADR-0017).

use agent_core::seams::{SeamError, SeamResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

/// Lifecycle of one task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    #[default]
    Pending,
    Claimed,
    Done,
}

impl TaskState {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskState::Pending => "pending",
            TaskState::Claimed => "claimed",
            TaskState::Done => "done",
        }
    }
}

/// One node of the DAG.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamTask {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub state: TaskState,
    #[serde(default)]
    pub claimed_by: Option<String>,
    #[serde(default)]
    pub result: Option<String>,
}

impl TeamTask {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            depends_on: Vec::new(),
            state: TaskState::Pending,
            claimed_by: None,
            result: None,
        }
    }

    pub fn after(mut self, deps: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.depends_on = deps.into_iter().map(Into::into).collect();
        self
    }
}

/// A validated team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentTeam {
    #[serde(default)]
    pub id: String,
    pub owner: String,
    pub name: String,
    pub tasks: Vec<TeamTask>,
}

impl AgentTeam {
    /// Build and validate in one step: nothing invalid ever reaches a store.
    pub fn new(
        owner: impl Into<String>,
        name: impl Into<String>,
        tasks: Vec<TeamTask>,
    ) -> SeamResult<Self> {
        let team = Self {
            id: String::new(),
            owner: owner.into(),
            name: name.into(),
            tasks,
        };
        team.validate()?;
        Ok(team)
    }

    fn validate(&self) -> SeamResult<()> {
        if self.owner.trim().is_empty() {
            return Err(SeamError::Invalid("team owner must not be empty".into()));
        }
        if self.name.trim().is_empty() {
            return Err(SeamError::Invalid("team name must not be empty".into()));
        }
        if self.tasks.is_empty() {
            return Err(SeamError::Invalid("a team needs at least one task".into()));
        }
        let mut ids = BTreeSet::new();
        for task in &self.tasks {
            if task.id.trim().is_empty() {
                return Err(SeamError::Invalid("task id must not be empty".into()));
            }
            if task.title.trim().is_empty() {
                return Err(SeamError::Invalid(format!(
                    "task `{}` needs a title",
                    task.id
                )));
            }
            if !ids.insert(task.id.as_str()) {
                return Err(SeamError::Invalid(format!(
                    "duplicate task id `{}`",
                    task.id
                )));
            }
        }
        for task in &self.tasks {
            for dep in &task.depends_on {
                if !ids.contains(dep.as_str()) {
                    return Err(SeamError::Invalid(format!(
                        "task `{}` depends on unknown task `{dep}`",
                        task.id
                    )));
                }
                if dep == &task.id {
                    return Err(SeamError::Invalid(format!(
                        "task `{}` depends on itself",
                        task.id
                    )));
                }
            }
        }
        if self.has_cycle() {
            return Err(SeamError::Invalid(
                "task dependency graph contains a cycle".into(),
            ));
        }
        Ok(())
    }

    /// Kahn's algorithm: a cycle leaves nodes with an unconsumed in-degree.
    fn has_cycle(&self) -> bool {
        let mut indegree: BTreeMap<&str, usize> = self
            .tasks
            .iter()
            .map(|t| (t.id.as_str(), t.depends_on.len()))
            .collect();
        let mut ready: Vec<&str> = self
            .tasks
            .iter()
            .filter(|t| t.depends_on.is_empty())
            .map(|t| t.id.as_str())
            .collect();
        let mut visited = 0usize;
        while let Some(id) = ready.pop() {
            visited += 1;
            for task in &self.tasks {
                if task.depends_on.iter().any(|d| d == id) {
                    if let Some(entry) = indegree.get_mut(task.id.as_str()) {
                        *entry = entry.saturating_sub(1);
                        if *entry == 0 {
                            ready.push(task.id.as_str());
                        }
                    }
                }
            }
        }
        visited != self.tasks.len()
    }

    pub fn task(&self, id: &str) -> Option<&TeamTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    fn task_mut(&mut self, id: &str) -> Option<&mut TeamTask> {
        self.tasks.iter_mut().find(|t| t.id == id)
    }

    /// Tasks whose dependencies are all done and which nobody claimed yet.
    pub fn ready(&self) -> Vec<&TeamTask> {
        self.tasks
            .iter()
            .filter(|t| t.state == TaskState::Pending && self.deps_done(t))
            .collect()
    }

    fn deps_done(&self, task: &TeamTask) -> bool {
        task.depends_on.iter().all(|dep| {
            self.task(dep)
                .map(|t| t.state == TaskState::Done)
                .unwrap_or(false)
        })
    }

    /// Claim a ready task. Refuses unknown, already-taken and blocked tasks.
    pub fn claim(&mut self, task_id: &str, worker: &str) -> SeamResult<()> {
        if worker.trim().is_empty() {
            return Err(SeamError::Invalid("worker identity is required".into()));
        }
        let deps_done = {
            let task = self
                .task(task_id)
                .ok_or_else(|| SeamError::NotFound(task_id.to_string()))?;
            match task.state {
                TaskState::Claimed => {
                    return Err(SeamError::Denied(format!(
                        "task `{task_id}` is already claimed by {}",
                        task.claimed_by.as_deref().unwrap_or("someone")
                    )))
                }
                TaskState::Done => {
                    return Err(SeamError::Denied(format!("task `{task_id}` is done")))
                }
                TaskState::Pending => self.deps_done(task),
            }
        };
        if !deps_done {
            let blocked: Vec<&str> = self
                .task(task_id)
                .map(|t| {
                    t.depends_on
                        .iter()
                        .filter(|d| {
                            self.task(d)
                                .map(|t| t.state != TaskState::Done)
                                .unwrap_or(true)
                        })
                        .map(String::as_str)
                        .collect()
                })
                .unwrap_or_default();
            return Err(SeamError::Denied(format!(
                "task `{task_id}` is blocked by unfinished tasks: {}",
                blocked.join(", ")
            )));
        }
        let task = self.task_mut(task_id).expect("checked above");
        task.state = TaskState::Claimed;
        task.claimed_by = Some(worker.to_string());
        Ok(())
    }

    /// Finish a claimed task and record its result.
    pub fn complete(&mut self, task_id: &str, result: impl Into<String>) -> SeamResult<()> {
        let task = self
            .task_mut(task_id)
            .ok_or_else(|| SeamError::NotFound(task_id.to_string()))?;
        if task.state != TaskState::Claimed {
            return Err(SeamError::Denied(format!(
                "task `{task_id}` must be claimed before it can be completed (state: {})",
                task.state.as_str()
            )));
        }
        task.state = TaskState::Done;
        task.result = Some(result.into());
        Ok(())
    }

    /// Release a claim so another worker can pick the task up.
    pub fn release(&mut self, task_id: &str) -> SeamResult<()> {
        let task = self
            .task_mut(task_id)
            .ok_or_else(|| SeamError::NotFound(task_id.to_string()))?;
        if task.state != TaskState::Claimed {
            return Err(SeamError::Denied(format!(
                "task `{task_id}` is not claimed (state: {})",
                task.state.as_str()
            )));
        }
        task.state = TaskState::Pending;
        task.claimed_by = None;
        Ok(())
    }

    pub fn progress(&self) -> (usize, usize) {
        let done = self
            .tasks
            .iter()
            .filter(|t| t.state == TaskState::Done)
            .count();
        (done, self.tasks.len())
    }

    pub fn is_finished(&self) -> bool {
        self.tasks.iter().all(|t| t.state == TaskState::Done)
    }

    /// One-line human/model readable status.
    pub fn render(&self) -> String {
        let (done, total) = self.progress();
        let mut out = format!(
            "team `{}` ({done}/{total} done{})\n",
            self.name,
            if self.id.is_empty() {
                String::new()
            } else {
                format!(", id {}", self.id)
            }
        );
        for task in &self.tasks {
            let deps = if task.depends_on.is_empty() {
                String::new()
            } else {
                format!(" after {}", task.depends_on.join(","))
            };
            let who = task
                .claimed_by
                .as_ref()
                .map(|w| format!(" by {w}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- [{}] {}{}{}\n",
                task.state.as_str(),
                task.title,
                deps,
                who
            ));
        }
        out.trim_end().to_string()
    }
}

/// Persistence for teams (in-memory now, Postgres in the cloud runtime).
#[async_trait]
pub trait TeamStore: Send + Sync {
    async fn create(&self, team: AgentTeam) -> SeamResult<AgentTeam>;
    async fn get(&self, owner: &str, id: &str) -> SeamResult<AgentTeam>;
    async fn list(&self, owner: &str) -> SeamResult<Vec<AgentTeam>>;
    async fn update(&self, team: AgentTeam) -> SeamResult<AgentTeam>;
}

/// In-memory team store, scoped by owner on every read.
#[derive(Default)]
pub struct MemoryTeams {
    teams: Mutex<BTreeMap<String, AgentTeam>>,
    seq: Mutex<u64>,
}

impl MemoryTeams {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> SeamResult<std::sync::MutexGuard<'_, BTreeMap<String, AgentTeam>>> {
        self.teams
            .lock()
            .map_err(|e| SeamError::Io(format!("team store poisoned: {e}")))
    }
}

#[async_trait]
impl TeamStore for MemoryTeams {
    async fn create(&self, mut team: AgentTeam) -> SeamResult<AgentTeam> {
        team.validate()?;
        let mut seq = self
            .seq
            .lock()
            .map_err(|e| SeamError::Io(format!("team store poisoned: {e}")))?;
        *seq += 1;
        team.id = format!("team-{seq}");
        self.lock()?.insert(team.id.clone(), team.clone());
        Ok(team)
    }

    async fn get(&self, owner: &str, id: &str) -> SeamResult<AgentTeam> {
        self.lock()?
            .get(id)
            .filter(|t| t.owner == owner)
            .cloned()
            .ok_or_else(|| SeamError::NotFound(format!("team {id}")))
    }

    async fn list(&self, owner: &str) -> SeamResult<Vec<AgentTeam>> {
        Ok(self
            .lock()?
            .values()
            .filter(|t| t.owner == owner)
            .cloned()
            .collect())
    }

    async fn update(&self, team: AgentTeam) -> SeamResult<AgentTeam> {
        team.validate()?;
        let mut guard = self.lock()?;
        let stored = guard
            .get(&team.id)
            .ok_or_else(|| SeamError::NotFound(format!("team {}", team.id)))?;
        if stored.owner != team.owner {
            return Err(SeamError::Denied(format!(
                "team {} belongs to another tenant",
                team.id
            )));
        }
        guard.insert(team.id.clone(), team.clone());
        Ok(team)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn team() -> AgentTeam {
        AgentTeam::new(
            "t1",
            "release",
            vec![
                TeamTask::new("spec", "write the spec"),
                TeamTask::new("impl", "implement").after(["spec"]),
                TeamTask::new("ship", "ship it").after(["impl"]),
            ],
        )
        .unwrap()
    }

    #[test]
    fn validation_rejects_dangling_duplicate_and_cyclic_graphs() {
        let dangling = AgentTeam::new("t", "n", vec![TeamTask::new("a", "a").after(["missing"])]);
        assert!(matches!(dangling, Err(SeamError::Invalid(_))));

        let duplicate = AgentTeam::new(
            "t",
            "n",
            vec![TeamTask::new("a", "a"), TeamTask::new("a", "b")],
        );
        assert!(matches!(duplicate, Err(SeamError::Invalid(_))));

        let cyclic = AgentTeam::new(
            "t",
            "n",
            vec![
                TeamTask::new("a", "a").after(["b"]),
                TeamTask::new("b", "b").after(["a"]),
            ],
        );
        assert!(matches!(cyclic, Err(SeamError::Invalid(_))));

        let selfish = AgentTeam::new("t", "n", vec![TeamTask::new("a", "a").after(["a"])]);
        assert!(matches!(selfish, Err(SeamError::Invalid(_))));

        assert!(matches!(
            AgentTeam::new("", "n", vec![TeamTask::new("a", "a")]),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            AgentTeam::new("t", " ", vec![TeamTask::new("a", "a")]),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            AgentTeam::new("t", "n", vec![]),
            Err(SeamError::Invalid(_))
        ));
        assert!(matches!(
            AgentTeam::new("t", "n", vec![TeamTask::new("a", " ")]),
            Err(SeamError::Invalid(_))
        ));
    }

    #[test]
    fn claims_are_dependency_gated() {
        let mut t = team();
        let ready: Vec<&str> = t.ready().iter().map(|x| x.id.as_str()).collect();
        assert_eq!(ready, vec!["spec"]);

        assert!(matches!(
            t.claim("impl", "w1"),
            Err(SeamError::Denied(_)) // blocked by `spec`
        ));
        t.claim("spec", "w1").unwrap();
        assert!(matches!(
            t.claim("spec", "w2"),
            Err(SeamError::Denied(_)) // already claimed
        ));

        t.complete("spec", "spec done").unwrap();
        assert!(matches!(
            t.complete("spec", "again"),
            Err(SeamError::Denied(_))
        ));
        assert_eq!(t.ready()[0].id, "impl");
        assert_eq!(t.progress(), (1, 3));
        assert!(!t.is_finished());
    }

    #[test]
    fn release_returns_a_task_to_the_pool() {
        let mut t = team();
        t.claim("spec", "w1").unwrap();
        t.release("spec").unwrap();
        assert_eq!(t.ready()[0].id, "spec");
        assert!(matches!(t.release("spec"), Err(SeamError::Denied(_))));
        assert!(matches!(t.claim("nope", "w"), Err(SeamError::NotFound(_))));
        assert!(matches!(t.claim("spec", " "), Err(SeamError::Invalid(_))));
        assert!(matches!(
            t.complete("nope", "x"),
            Err(SeamError::NotFound(_))
        ));
    }

    #[test]
    fn finishing_the_dag_is_observable() {
        let mut t = team();
        for id in ["spec", "impl", "ship"] {
            t.claim(id, "w").unwrap();
            t.complete(id, format!("{id} ok")).unwrap();
        }
        assert!(t.is_finished());
        assert_eq!(t.progress(), (3, 3));
        let rendered = t.render();
        assert!(rendered.contains("3/3 done"), "{rendered}");
        assert!(rendered.contains("[done] ship it"), "{rendered}");
    }

    #[tokio::test]
    async fn store_is_owner_scoped_and_ids_are_assigned() {
        let store = MemoryTeams::new();
        let created = store.create(team()).await.unwrap();
        assert_eq!(created.id, "team-1");

        let other = store.create(team()).await.unwrap();
        assert_eq!(other.id, "team-2");

        assert_eq!(store.list("t1").await.unwrap().len(), 2);
        assert!(store.list("t2").await.unwrap().is_empty());
        assert!(matches!(
            store.get("t2", "team-1").await,
            Err(SeamError::NotFound(_))
        ));
        assert_eq!(store.get("t1", "team-1").await.unwrap().name, "release");

        let mut stolen = store.get("t1", "team-1").await.unwrap();
        stolen.owner = "t2".into();
        assert!(matches!(
            store.update(stolen).await,
            Err(SeamError::Denied(_))
        ));

        // An invalid graph can never be written back.
        let mut broken = store.get("t1", "team-1").await.unwrap();
        broken.tasks[0].depends_on = vec!["ghost".into()];
        assert!(matches!(
            store.update(broken).await,
            Err(SeamError::Invalid(_))
        ));
    }

    #[tokio::test]
    async fn updating_an_unknown_team_is_not_found() {
        let store = MemoryTeams::new();
        let mut t = team();
        t.id = "missing".into();
        assert!(matches!(store.update(t).await, Err(SeamError::NotFound(_))));
    }
}
