//! Recursive self-improvement over HTTP.
//!
//! Thin handlers: validate → call the RSI seam → render. Two invariants are
//! enforced here, and nowhere else:
//!
//! * **Nothing publishes implicitly.** `POST /v1/rsi/proposals/{id}/promote` is
//!   the only path from "pending" to "in force", and it carries the revision the
//!   approver believes is current. A stale expectation is `409`, never a
//!   last-writer-wins overwrite.
//! * **Cross-tenant reuse must be adapted.** `POST /v1/rsi/playbooks/{key}/adapt`
//!   is how a playbook learned in one tenant reaches another; without a complete
//!   adaptation record it answers `422` and creates nothing.
//!
//! Endpoints:
//!
//! * `GET  /v1/rsi/status` — is it on, which gate, how much is buffered
//! * `POST /v1/rsi/sweep` — run a sweep now (the external-cron trigger)
//! * `GET  /v1/rsi/reports`, `GET /v1/rsi/reports/{id}` — diagnoses + evidence
//! * `GET  /v1/rsi/proposals` — pending candidates
//! * `POST /v1/rsi/proposals/{id}/promote|discard` — the human gate
//! * `GET  /v1/rsi/playbooks/{key}`, `GET /v1/rsi/playbooks/{key}/versions`
//! * `POST /v1/rsi/playbooks/{key}/adapt` — propose an adapted cross-tenant copy

use agent_core::seams::{Adaptation, Playbook, PlaybookCandidate, Proposal, RsiScope, SeamError};
use agent_core::session::insight::{Evidence, Finding};
use axum::extract::{Path, Query, State};
use axum::Json;
use serde::{Deserialize, Serialize};

use crate::app::AppState;
use crate::auth::{Principal, PrincipalKind};
use crate::store::ReportRow;
use crate::types::ListResponse;
use crate::AppError;

/// Domain used when the caller does not name one, so a single-tenant deployment
/// never has to pass a query parameter it does not have.
const DEFAULT_DOMAIN: &str = "default";
const DEFAULT_LIMIT: i64 = 50;
const MAX_LIMIT: i64 = 200;

/// Map a seam failure onto the HTTP surface with a stable status code.
fn rsi_error(e: SeamError) -> AppError {
    match e {
        SeamError::Invalid(message) => AppError::BadRequest(message),
        SeamError::NotFound(_) => AppError::NotFound,
        SeamError::Denied(message) => AppError::Forbidden(message),
        SeamError::Conflict(message) => AppError::Conflict(message),
        SeamError::Unadapted(message) => AppError::Unprocessable(message),
        SeamError::Unavailable(message) => AppError::Unavailable(message),
        other => AppError::Core(agent_core::CoreError::Config(other.to_string())),
    }
}

fn clamp_limit(limit: Option<i64>) -> i64 {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn scope_of(principal: &Principal, domain: Option<&str>) -> RsiScope {
    let domain = domain
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_DOMAIN);
    RsiScope::new(principal.id.clone(), domain)
}

fn sweep_of(state: &AppState) -> Result<std::sync::Arc<aria_agent_rsi::RsiSweep>, AppError> {
    state.rsi.sweep.clone().ok_or_else(|| {
        AppError::Unavailable(
            "recursive self-improvement is not enabled in this deployment \
             (start it with --rsi-enabled)"
                .into(),
        )
    })
}

// ---------------------------------------------------------------------------
// Views (wire only: the tenant id never appears)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct StatusView {
    pub object: &'static str,
    pub enabled: bool,
    /// `human` | `auto` | `broker`, or `null` when no sweep is configured.
    pub gate: Option<&'static str>,
    /// Whether a real re-run confirms offline-validated proposals.
    pub real_rerun: bool,
    pub sessions: usize,
    pub records: usize,
    pub unhealthy_sessions: usize,
}

#[derive(Debug, Serialize)]
pub struct ReportView {
    pub id: String,
    pub object: &'static str,
    pub session: String,
    pub score: i32,
    pub truncated: bool,
    pub findings: Vec<Finding>,
    pub created_at_ms: i64,
}

impl From<ReportRow> for ReportView {
    fn from(row: ReportRow) -> Self {
        Self {
            id: row.id,
            object: "rsi_report",
            session: row.session,
            score: row.score,
            truncated: row.truncated,
            findings: row.findings,
            created_at_ms: row.created_at_ms,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ScopeView {
    pub domain: String,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub params: std::collections::BTreeMap<String, String>,
}

impl From<&RsiScope> for ScopeView {
    fn from(scope: &RsiScope) -> Self {
        Self {
            domain: scope.domain.clone(),
            params: scope.params.clone(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct ProposalView {
    pub id: String,
    pub object: &'static str,
    pub key: String,
    pub pathology: String,
    pub tool: Option<String>,
    pub title: String,
    pub body: String,
    pub score: i32,
    pub baseline: i32,
    pub scope: ScopeView,
    /// Where the evidence came from; differs from `scope` only for an adapted
    /// cross-tenant copy.
    pub origin: ScopeView,
    pub evidence: Vec<Evidence>,
    pub created_at_ms: i64,
    /// True when the candidate came from another tenant and carries an
    /// adaptation record.
    pub adapted: bool,
}

impl From<Proposal> for ProposalView {
    fn from(proposal: Proposal) -> Self {
        let key = proposal.key();
        Self {
            id: proposal.id,
            object: "rsi_proposal",
            key,
            pathology: proposal.pathology,
            tool: proposal.tool,
            title: proposal.title,
            body: proposal.body,
            score: proposal.score,
            baseline: proposal.baseline,
            scope: ScopeView::from(&proposal.scope),
            origin: ScopeView::from(&proposal.origin),
            evidence: proposal.evidence,
            created_at_ms: proposal.created_at_ms,
            adapted: proposal.adaptation.is_some(),
        }
    }
}

#[derive(Debug, Serialize)]
pub struct PlaybookView {
    pub object: &'static str,
    pub key: String,
    pub revision: u64,
    pub title: String,
    pub body: String,
    pub score: i32,
    pub scope: ScopeView,
    pub origin: ScopeView,
    pub adapted: bool,
    pub updated_at_ms: i64,
}

impl From<Playbook> for PlaybookView {
    fn from(playbook: Playbook) -> Self {
        Self {
            object: "rsi_playbook",
            key: playbook.key,
            revision: playbook.revision,
            title: playbook.title,
            body: playbook.body,
            score: playbook.score,
            scope: ScopeView::from(&playbook.scope),
            origin: ScopeView::from(&playbook.origin),
            adapted: playbook.adaptation.is_some(),
            updated_at_ms: playbook.updated_at_ms,
        }
    }
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

#[derive(Debug, Default, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SweepRequest {
    /// Sweep one session instead of every session that reached the threshold.
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PromoteRequest {
    /// The revision the approver believes is current (`0` when none exists).
    pub expected_revision: u64,
}

#[derive(Debug, Deserialize)]
pub struct AdaptRequest {
    /// Domain to publish into (the tenant is always the caller's).
    pub domain: String,
    /// Required: what changed and why the copy is valid there.
    pub adaptation: Adaptation,
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

pub async fn status(State(state): State<AppState>) -> Result<Json<StatusView>, AppError> {
    let stats = state
        .rsi
        .ledger
        .as_ref()
        .map(|ledger| ledger.stats())
        .unwrap_or_default();
    Ok(Json(StatusView {
        object: "rsi_status",
        enabled: state.rsi.enabled(),
        gate: state.rsi.sweep.as_ref().map(|sweep| sweep.gate().label()),
        real_rerun: state.rsi.config.real_rerun,
        sessions: stats.sessions,
        records: stats.records,
        unhealthy_sessions: stats.unhealthy_sessions,
    }))
}

pub async fn sweep(
    State(state): State<AppState>,
    principal: Principal,
    body: Option<Json<SweepRequest>>,
) -> Result<Json<aria_agent_rsi::SweepOutcome>, AppError> {
    let sweep = sweep_of(&state)?;
    let body = body.map(|Json(body)| body).unwrap_or_default();
    let scope = scope_of(&principal, body.domain.as_deref());
    let outcome = match body.session.as_deref() {
        Some(session) => sweep.sweep_session(&scope, session).await,
        None => sweep.sweep_principal(&scope).await,
    }
    .map_err(rsi_error)?;
    Ok(Json(outcome))
}

pub async fn list_reports(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse<ReportView>>, AppError> {
    let rows = state
        .rsi
        .store
        .list_reports(&principal, clamp_limit(query.limit))
        .await?;
    Ok(Json(ListResponse {
        object: "list".into(),
        data: rows.into_iter().map(ReportView::from).collect(),
        has_more: false,
        first_id: None,
        last_id: None,
    }))
}

pub async fn get_report(
    State(state): State<AppState>,
    principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<ReportView>, AppError> {
    let row = state.rsi.store.get_report(&principal, &id).await?;
    Ok(Json(ReportView::from(row)))
}

pub async fn list_proposals(
    State(state): State<AppState>,
    principal: Principal,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse<ProposalView>>, AppError> {
    let scope = scope_of(&principal, query.domain.as_deref());
    let pending = state
        .rsi
        .playbooks
        .pending(&scope)
        .await
        .map_err(rsi_error)?;
    let limit = clamp_limit(query.limit) as usize;
    let data: Vec<ProposalView> = pending
        .into_iter()
        .take(limit)
        .map(ProposalView::from)
        .collect();
    Ok(Json(ListResponse {
        object: "list".into(),
        data,
        has_more: false,
        first_id: None,
        last_id: None,
    }))
}

pub async fn promote(
    State(state): State<AppState>,
    _principal: Principal,
    Path(id): Path<String>,
    Json(body): Json<PromoteRequest>,
) -> Result<Json<PlaybookView>, AppError> {
    let playbook = state
        .rsi
        .playbooks
        .promote(&id, body.expected_revision)
        .await
        .map_err(rsi_error)?;
    Ok(Json(PlaybookView::from(playbook)))
}

pub async fn discard(
    State(state): State<AppState>,
    _principal: Principal,
    Path(id): Path<String>,
) -> Result<Json<ProposalView>, AppError> {
    let proposal = state.rsi.playbooks.discard(&id).await.map_err(rsi_error)?;
    Ok(Json(ProposalView::from(proposal)))
}

pub async fn current_playbook(
    State(state): State<AppState>,
    principal: Principal,
    Path(key): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<PlaybookView>, AppError> {
    let scope = scope_of(&principal, query.domain.as_deref());
    let playbook = state
        .rsi
        .playbooks
        .current(&scope, &key)
        .await
        .map_err(rsi_error)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(PlaybookView::from(playbook)))
}

pub async fn list_versions(
    State(state): State<AppState>,
    principal: Principal,
    Path(key): Path<String>,
    Query(query): Query<ListQuery>,
) -> Result<Json<ListResponse<PlaybookView>>, AppError> {
    let scope = scope_of(&principal, query.domain.as_deref());
    let versions = state
        .rsi
        .playbooks
        .list_versions(&scope, &key)
        .await
        .map_err(rsi_error)?;
    Ok(Json(ListResponse {
        object: "list".into(),
        data: versions.into_iter().map(PlaybookView::from).collect(),
        has_more: false,
        first_id: None,
        last_id: None,
    }))
}

/// Propose a copy of another placement's current playbook, with the adaptation
/// record that justifies it.
///
/// The result is only a **pending proposal**: adapting is not publishing. Reading
/// another tenant's playbook requires an admin caller, so a tenant cannot probe
/// for the existence of someone else's capability.
pub async fn adapt(
    State(state): State<AppState>,
    principal: Principal,
    Path(key): Path<String>,
    Json(body): Json<AdaptRequest>,
) -> Result<Json<ProposalView>, AppError> {
    let origin = body.adaptation.source_scope();
    if origin.principal != principal.id && principal.kind != PrincipalKind::Admin {
        return Err(AppError::Forbidden(
            "only an admin can adapt a playbook from another tenant".into(),
        ));
    }
    let source = state
        .rsi
        .playbooks
        .current(&origin, &key)
        .await
        .map_err(rsi_error)?
        .ok_or(AppError::NotFound)?;
    let target = scope_of(&principal, Some(&body.domain));
    let candidate = PlaybookCandidate {
        proposal_id: uuid::Uuid::new_v4().to_string(),
        origin: origin.clone(),
        scope: target,
        key: source.key.clone(),
        title: source.title.clone(),
        body: source.body.clone(),
        score: source.score,
        // The copy claims no measured gain of its own: the improvement is the
        // recorded adaptation, and the replay cannot verify another tenant's
        // history. A baseline one point lower expresses exactly that.
        baseline: source.score.saturating_sub(1),
        evidence: Vec::new(),
        adaptation: Some(body.adaptation),
    };
    let proposal = state
        .rsi
        .playbooks
        .submit(candidate)
        .await
        .map_err(rsi_error)?;
    Ok(Json(ProposalView::from(proposal)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse;

    fn status_of(error: AppError) -> axum::http::StatusCode {
        error.into_response().status()
    }

    /// These status codes are what a client retries on, so each seam failure maps
    /// to exactly one of them.
    #[test]
    fn every_seam_failure_maps_onto_a_stable_status() {
        use axum::http::StatusCode;
        assert_eq!(
            status_of(rsi_error(SeamError::Invalid("x".into()))),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status_of(rsi_error(SeamError::NotFound("x".into()))),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            status_of(rsi_error(SeamError::Denied("x".into()))),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            status_of(rsi_error(SeamError::Conflict("x".into()))),
            StatusCode::CONFLICT
        );
        assert_eq!(
            status_of(rsi_error(SeamError::Unadapted("x".into()))),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            status_of(rsi_error(SeamError::Unavailable("x".into()))),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            status_of(rsi_error(SeamError::Io("x".into()))),
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }

    #[test]
    fn list_limits_are_clamped_to_the_documented_window() {
        assert_eq!(clamp_limit(None), DEFAULT_LIMIT);
        assert_eq!(clamp_limit(Some(0)), 1);
        assert_eq!(clamp_limit(Some(5)), 5);
        assert_eq!(clamp_limit(Some(10_000)), MAX_LIMIT);
    }

    #[test]
    fn an_absent_domain_falls_back_to_default() {
        let principal = Principal::admin();
        assert_eq!(scope_of(&principal, None).domain, DEFAULT_DOMAIN);
        assert_eq!(scope_of(&principal, Some("  ")).domain, DEFAULT_DOMAIN);
        assert_eq!(scope_of(&principal, Some("billing")).domain, "billing");
        assert_eq!(scope_of(&principal, Some("billing")).principal, "admin");
    }
}
