//! Agent teams and scheduled jobs over HTTP.
//!
//! Thin handlers: validate → call the orchestrator seam → render. Tenant
//! scoping lives in the providers (`principal_id` on every row), so an id owned
//! by another tenant is indistinguishable from a missing one on read and is
//! refused on write.
//!
//! * `POST|GET /v1/agent-teams`, `GET /v1/agent-teams/{id}`,
//!   `POST /v1/agent-teams/{id}/tasks/{task}/claim|complete`
//! * `POST|GET /v1/scheduled-jobs`, `DELETE /v1/scheduled-jobs/{id}`

use agent_core::seams::{ScheduleEntry, SeamError};
use aria_agent_orchestrator::teams::{AgentTeam, TeamTask};
use axum::extract::{Path, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::auth::Principal;
use crate::types::ListResponse;
use crate::AppError;

/// Wire view of a team (the tenant id stays internal).
#[derive(Debug, Serialize)]
pub struct TeamView {
    pub id: String,
    pub object: &'static str,
    pub name: String,
    pub tasks: Vec<TeamTask>,
    pub done: usize,
    pub total: usize,
}

impl TeamView {
    fn new(team: AgentTeam) -> Self {
        let (done, total) = team.progress();
        Self {
            id: team.id,
            object: "agent_team",
            name: team.name,
            tasks: team.tasks,
            done,
            total,
        }
    }
}

/// Wire view of a scheduled job (epoch milliseconds, like the rest of the API).
#[derive(Debug, Serialize)]
pub struct JobView {
    pub id: String,
    pub object: &'static str,
    pub name: String,
    pub every_secs: Option<u64>,
    pub at_ms: Option<i64>,
    pub payload: serde_json::Value,
    pub enabled: bool,
    pub next_run_ms: Option<i64>,
}

impl From<ScheduleEntry> for JobView {
    fn from(entry: ScheduleEntry) -> Self {
        Self {
            id: entry.id,
            object: "scheduled_job",
            name: entry.name,
            every_secs: entry.every_secs,
            at_ms: entry.at_ms,
            payload: entry.payload,
            enabled: entry.enabled,
            next_run_ms: entry.next_run_ms,
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct TaskSpec {
    pub id: Option<String>,
    pub title: String,
    #[serde(default)]
    pub depends_on: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateTeamRequest {
    pub name: String,
    pub tasks: Vec<TaskSpec>,
}

#[derive(Debug, Deserialize)]
pub struct CompleteTaskRequest {
    #[serde(default)]
    pub result: String,
    pub worker: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClaimTaskRequest {
    pub worker: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CreateJobRequest {
    pub name: String,
    pub every_secs: Option<u64>,
    pub at_ms: Option<i64>,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// Map a seam error onto the API error surface.
fn seam_error(e: SeamError) -> AppError {
    match e {
        SeamError::Invalid(m) => AppError::BadRequest(m),
        SeamError::NotFound(m) => {
            tracing::debug!(target: "orchestration", %m, "not found");
            AppError::NotFound
        }
        SeamError::Denied(m) => AppError::Forbidden(m),
        // A missing provider is a deployment problem, not a client one.
        other => AppError::Core(agent_core::CoreError::Config(other.to_string())),
    }
}

pub async fn create_team(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateTeamRequest>,
) -> Result<Json<TeamView>, AppError> {
    let tasks = body
        .tasks
        .into_iter()
        .enumerate()
        .map(|(index, spec)| {
            let id = spec.id.unwrap_or_else(|| format!("t{}", index + 1));
            TeamTask::new(id, spec.title).after(spec.depends_on)
        })
        .collect();
    let team = AgentTeam::new(principal.id.clone(), body.name, tasks).map_err(seam_error)?;
    let stored = state.teams.create(team).await.map_err(seam_error)?;
    Ok(Json(TeamView::new(stored)))
}

pub async fn list_teams(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<ListResponse<TeamView>>, AppError> {
    let teams = state.teams.list(&principal.id).await.map_err(seam_error)?;
    Ok(Json(ListResponse {
        object: "list".into(),
        data: teams.into_iter().map(TeamView::new).collect(),
        has_more: false,
        first_id: None,
        last_id: None,
    }))
}

pub async fn get_team(
    State(state): State<AppState>,
    principal: Principal,
    Path(team_id): Path<String>,
) -> Result<Json<TeamView>, AppError> {
    let team = state
        .teams
        .get(&principal.id, &team_id)
        .await
        .map_err(seam_error)?;
    Ok(Json(TeamView::new(team)))
}

pub async fn claim_task(
    State(state): State<AppState>,
    principal: Principal,
    Path((team_id, task_id)): Path<(String, String)>,
    body: Option<Json<ClaimTaskRequest>>,
) -> Result<Json<TeamView>, AppError> {
    let worker = body
        .and_then(|Json(b)| b.worker)
        .unwrap_or_else(|| principal.id.clone());
    let mut team = state
        .teams
        .get(&principal.id, &team_id)
        .await
        .map_err(seam_error)?;
    team.claim(&task_id, &worker).map_err(seam_error)?;
    let updated = state.teams.update(team).await.map_err(seam_error)?;
    Ok(Json(TeamView::new(updated)))
}

pub async fn complete_task(
    State(state): State<AppState>,
    principal: Principal,
    Path((team_id, task_id)): Path<(String, String)>,
    Json(body): Json<CompleteTaskRequest>,
) -> Result<Json<TeamView>, AppError> {
    let mut team = state
        .teams
        .get(&principal.id, &team_id)
        .await
        .map_err(seam_error)?;
    team.complete(&task_id, body.result).map_err(seam_error)?;
    let updated = state.teams.update(team).await.map_err(seam_error)?;
    Ok(Json(TeamView::new(updated)))
}

pub async fn create_job(
    State(state): State<AppState>,
    principal: Principal,
    Json(body): Json<CreateJobRequest>,
) -> Result<Json<JobView>, AppError> {
    let mut entry = match (body.every_secs, body.at_ms) {
        (Some(secs), None) => ScheduleEntry::every(principal.id.clone(), body.name, secs),
        (None, Some(at)) => ScheduleEntry::at(principal.id.clone(), body.name, at),
        _ => {
            return Err(AppError::BadRequest(
                "exactly one of `every_secs` / `at_ms` is required".into(),
            ))
        }
    };
    entry.payload = body.payload;
    let stored = state.schedule.upsert(entry).await.map_err(seam_error)?;
    Ok(Json(stored.into()))
}

pub async fn list_jobs(
    State(state): State<AppState>,
    principal: Principal,
) -> Result<Json<ListResponse<JobView>>, AppError> {
    let jobs = state
        .schedule
        .list(&principal.id)
        .await
        .map_err(seam_error)?;
    Ok(Json(ListResponse {
        object: "list".into(),
        data: jobs.into_iter().map(JobView::from).collect(),
        has_more: false,
        first_id: None,
        last_id: None,
    }))
}

pub async fn delete_job(
    State(state): State<AppState>,
    principal: Principal,
    Path(job_id): Path<String>,
) -> Result<Json<JobView>, AppError> {
    let jobs = state
        .schedule
        .list(&principal.id)
        .await
        .map_err(seam_error)?;
    let job = jobs
        .into_iter()
        .find(|j| j.id == job_id)
        .ok_or(AppError::NotFound)?;
    if !state
        .schedule
        .remove(&principal.id, &job_id)
        .await
        .map_err(seam_error)?
    {
        return Err(AppError::NotFound);
    }
    Ok(Json(job.into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seam_errors_map_by_variant() {
        assert!(matches!(
            seam_error(SeamError::Invalid("x".into())),
            AppError::BadRequest(_)
        ));
        assert!(matches!(
            seam_error(SeamError::NotFound("x".into())),
            AppError::NotFound
        ));
        assert!(matches!(
            seam_error(SeamError::Denied("x".into())),
            AppError::Forbidden(_)
        ));
        assert!(matches!(
            seam_error(SeamError::Io("x".into())),
            AppError::Core(_)
        ));
    }
}
