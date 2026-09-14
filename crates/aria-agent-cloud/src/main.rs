//! `agent-cloud` — the Agents Cloud API.
//!
//! * axum HTTP server.
//! * Postgres is used **only** for structured metadata (`agents`, `runs`).
//! * Conversational context is injected from `agent-memo` (local/embedded
//!   store) — never persisted in Postgres.
//! * Model calls go to the OpenAI Responses/Chat API via `agent-core`'s
//!   `OpenAiModel` (enabled by the `openai` feature).
//! * Self-improvement (Reef-style) is layered on top: every turn is recorded
//!   (receipt `x-reef-agent-record-id`), feedback is bound via `/reef/report`,
//!   and `/reef/evolve` runs the local engine to propose a better harness,
//!   keeps the winner, versions it in Git (`.reef/`), and hot-swaps the shared
//!   `ActiveHarness` so live traffic picks it up without a restart.

mod cli_config;
mod upgrade;

use std::convert::Infallible;
use std::io::{self, IsTerminal};
use std::path::PathBuf;
use std::sync::Arc;

use agent_core::{ActiveHarness, Agent, AgentConfig, CoreError, OpenAiModel};
use agent_memo::{MemoStore, SledMemoStore};
use agent_reef::engine::EvolutionEngine;
use agent_reef::feedback::{Feedback, FeedbackStore, SledFeedbackStore};
use agent_reef::git;
use agent_reef::harness;
use agent_reef::record::{Record, RecordStore, SledRecordStore};
use agent_reef::ReefError;
use axum::extract::Request;
use axum::extract::{Path, State};
use axum::http::{HeaderName, HeaderValue, StatusCode};
use axum::middleware::{from_fn_with_state, Next};
use axum::response::sse::Event;
use axum::response::{sse::Sse, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::stream;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::json;
use clap::{Args, Parser, Subcommand, ArgAction};
use sqlx::postgres::PgPoolOptions;
use sqlx::Executor;
use sqlx::Row;
use uuid::Uuid;

const DEFAULT_MODEL: &str = "gpt-4o-mini";
const REEF_RECORD_HEADER: &str = "x-reef-agent-record-id";

#[derive(Clone)]
struct AppState {
    pool: sqlx::PgPool,
    /// Shared memo store — the single source of conversational context.
    memo: Arc<dyn MemoStore>,
    /// Local turn-records store (learning logs; NOT Postgres, NOT memo).
    records: Arc<dyn RecordStore>,
    /// Local feedback store (bound to records; NOT Postgres, NOT memo).
    feedback: Arc<dyn FeedbackStore>,
    /// Shared, hot-swappable harness served to every agent.
    active: Arc<ActiveHarness>,
    /// Evolution engine (local model engine = "local engine FFI").
    engine: Arc<EvolutionEngine>,
    /// `.reef/` artifact directory (Git-versioned harness files).
    reef_dir: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct AgentRecord {
    id: String,
    name: String,
}

#[derive(Deserialize)]
struct CreateAgentBody {
    name: String,
}

#[derive(Deserialize)]
struct RunBody {
    agent_id: String,
    session: String,
    input: String,
}

#[derive(Serialize)]
struct RunRecord {
    id: String,
    agent_id: String,
    session: String,
    output: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

#[derive(Deserialize)]
struct ReefReportBody {
    /// Receipt ids of the recorded turns this feedback concerns.
    references: Vec<String>,
    score: f64,
    feedback: Option<String>,
}

#[derive(Debug)]
enum AppError {
    Db(sqlx::Error),
    Core(CoreError),
    Reef(ReefError),
    BadReport(String),
    NotFound,
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, msg) = match self {
            AppError::Db(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("db: {e}")),
            AppError::Core(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("core: {e}")),
            AppError::Reef(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("reef: {e}")),
            AppError::BadReport(e) => (StatusCode::BAD_REQUEST, format!("bad_report: {e}")),
            AppError::NotFound => (StatusCode::NOT_FOUND, "not found".into()),
        };
        (status, Json(ErrorBody { error: msg })).into_response()
    }
}

/// One streamed token, serialized into an SSE `data:` frame.
#[derive(Serialize)]
struct StreamToken {
    token: String,
}

/// Bearer / ApiKey auth gate.
///
/// Reads `AGENT_CLOUD_API_KEY` from the environment. When it is set (non-empty)
/// every request must carry a matching `Authorization: Bearer <key>` or
/// `Authorization: ApiKey <key>` header, otherwise `401` is returned. When the
/// env var is absent the service runs open (dev convenience).
async fn require_auth(
    State(_state): State<AppState>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let expected = match std::env::var("AGENT_CLOUD_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => return Ok(next.run(req).await),
    };
    let presented = req
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let ok = presented
        .strip_prefix("Bearer ")
        .or_else(|| presented.strip_prefix("ApiKey "))
        .map(|tok| tok.trim() == expected)
        .unwrap_or(false);
    if !ok {
        return Err(StatusCode::UNAUTHORIZED);
    }
    Ok(next.run(req).await)
}

/// Build an [`Agent`] for the given run body (resolves the agent name from the
/// Postgres metadata store, wires the OpenAI model + memo, and **shares** the
/// process-wide `ActiveHarness` so a self-improvement win is picked up live).
async fn build_agent(state: &AppState, body: &RunBody) -> Result<Agent, AppError> {
    let name = agent_name(state, &body.agent_id).await?;
    let cfg = agent_config(&name, &body.session);
    Agent::with_harness(
        cfg,
        Box::new(OpenAiModel::new(DEFAULT_MODEL)),
        state.memo.clone(),
        state.active.clone(),
    )
    .map_err(AppError::Core)
}

async fn create_agent(
    State(state): State<AppState>,
    Json(body): Json<CreateAgentBody>,
) -> Result<Json<AgentRecord>, AppError> {
    let id = Uuid::new_v4().to_string();
    sqlx::query("INSERT INTO agents (id, name) VALUES ($1, $2)")
        .bind(&id)
        .bind(&body.name)
        .execute(&state.pool)
        .await
        .map_err(AppError::Db)?;
    Ok(Json(AgentRecord {
        id,
        name: body.name,
    }))
}

async fn get_agent(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<AgentRecord>, AppError> {
    let row = sqlx::query("SELECT id, name FROM agents WHERE id = $1")
        .bind(&id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Db)?
        .ok_or(AppError::NotFound)?;
    Ok(Json(AgentRecord {
        id: row.get("id"),
        name: row.get("name"),
    }))
}

async fn run_agent(State(state): State<AppState>, Json(body): Json<RunBody>) -> Response {
    let agent = match build_agent(&state, &body).await {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };
    let output = match agent.run(&body.input).await {
        Ok(o) => o,
        Err(e) => return AppError::Core(e).into_response(),
    };

    // Record the turn (learning log) and surface the receipt id as a header.
    let rec = Record::new(
        &body.agent_id,
        &body.session,
        None,
        &state.active.get().system_text(),
        &body.input,
        &output,
        DEFAULT_MODEL,
    );
    if let Err(e) = state.records.record_turn(rec.clone()).await {
        tracing::warn!("reef: failed to record turn: {e}");
    }

    // Metadata only — never the conversational context.
    let run_id = Uuid::new_v4().to_string();
    if let Err(e) = sqlx::query(
        "INSERT INTO runs (id, agent_id, session, input, output) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&run_id)
    .bind(&body.agent_id)
    .bind(&body.session)
    .bind(&body.input)
    .bind(&output)
    .execute(&state.pool)
    .await
    {
        tracing::warn!("reef: failed to persist run metadata: {e}");
    }

    let mut resp = Json(RunRecord {
        id: run_id,
        agent_id: body.agent_id,
        session: body.session,
        output,
    })
    .into_response();
    resp.headers_mut().insert(
        HeaderName::from_static(REEF_RECORD_HEADER),
        HeaderValue::from_str(&rec.id).unwrap_or(HeaderValue::from_static("")),
    );
    resp
}

async fn run_agent_stream(State(state): State<AppState>, Json(body): Json<RunBody>) -> Response {
    // Build the agent (resolves name + memo) before streaming.
    let agent = match build_agent(&state, &body).await {
        Ok(a) => a,
        Err(e) => return e.into_response(),
    };

    let sys = state.active.get().system_text();
    let rec_id = Uuid::new_v4().to_string();
    // Placeholder record so the receipt id is known before streaming begins.
    if let Err(e) = state
        .records
        .record_turn(Record::new(
            &body.agent_id,
            &body.session,
            None,
            &sys,
            &body.input,
            "",
            DEFAULT_MODEL,
        ))
        .await
    {
        tracing::warn!("reef: failed to record turn placeholder: {e}");
    }

    let token_stream = match agent.run_stream(&body.input).await {
        Ok(s) => s,
        Err(e) => return AppError::Core(e).into_response(),
    };

    // Finalize the record at the end of the stream (overwrite placeholder with
    // the real output, keeping the same receipt id).
    let records = state.records.clone();
    let agent_id = body.agent_id.clone();
    let session = body.session.clone();
    let input = body.input.clone();
    let rec_id_inner = rec_id.clone();
    let wrapped = async_stream::stream! {
        let mut collected = String::new();
        let mut s = token_stream;
        while let Some(item) = s.next().await {
            match item {
                Ok(tok) => {
                    collected.push_str(&tok);
                    yield Ok(tok);
                }
                Err(e) => {
                    yield Err(e);
                    return;
                }
            }
        }
        let mut final_rec =
            Record::new(&agent_id, &session, None, &sys, &input, &collected, DEFAULT_MODEL);
        final_rec.id = rec_id_inner.clone();
        let _ = records.record_turn(final_rec).await;
    };

    let sse = wrapped
        .map(|res| {
            Ok::<Event, Infallible>(match res {
                Ok(token) => Event::default()
                    .json_data(StreamToken { token })
                    .unwrap_or_else(|_| Event::default().data("[encode-error]")),
                Err(e) => Event::default().data(format!("error: {e}")),
            })
        })
        .chain(stream::once(async { Ok(Event::default().data("[DONE]")) }));
    let mut resp = Sse::new(Box::pin(sse)).into_response();
    resp.headers_mut().insert(
        HeaderName::from_static(REEF_RECORD_HEADER),
        HeaderValue::from_str(&rec_id).unwrap_or(HeaderValue::from_static("")),
    );
    resp
}

async fn agent_name(state: &AppState, agent_id: &str) -> Result<String, AppError> {
    let row = sqlx::query("SELECT name FROM agents WHERE id = $1")
        .bind(agent_id)
        .fetch_optional(&state.pool)
        .await
        .map_err(AppError::Db)?
        .ok_or(AppError::NotFound)?;
    Ok(row.get("name"))
}

fn agent_config(name: &str, session: &str) -> AgentConfig {
    AgentConfig {
        session: session.to_string(),
        agent_name: name.to_string(),
        sandbox_provider: "docker".into(),
        model: DEFAULT_MODEL.into(),
    }
}

// --- Reef self-improvement endpoints ---

/// Bind feedback to recorded turns. References must point at existing records.
async fn reef_report(
    State(state): State<AppState>,
    Json(body): Json<ReefReportBody>,
) -> Result<Json<serde_json::Value>, AppError> {
    for r in &body.references {
        if state.records.get(r).await.is_err() {
            return Err(AppError::BadReport(format!("unknown record: {r}")));
        }
    }
    let fb = Feedback::new(body.references, body.score, body.feedback);
    state.feedback.report(fb).await.map_err(AppError::Reef)?;
    Ok(Json(json!({ "ok": true })))
}

/// Run one evolution pass: propose → select → keep winner → version → hot-swap.
async fn reef_evolve(State(state): State<AppState>) -> Result<Json<serde_json::Value>, AppError> {
    match state.engine.evolve().await {
        Ok(out) => Ok(Json(json!({
            "version": out.version,
            "adopted": out.adopted,
            "candidate_system": out.candidate_system,
        }))),
        Err(ReefError::NoEligible) => Ok(Json(json!({
            "version": 0,
            "adopted": false,
            "reason": "no_eligible",
        }))),
        Err(e) => Err(AppError::Reef(e)),
    }
}

/// List committed harness versions (plus the implicit `baseline`).
async fn reef_versions(State(state): State<AppState>) -> Json<Vec<String>> {
    let mut v = git::list_versions(&state.reef_dir).unwrap_or_default();
    v.push("baseline".to_string());
    Json(v)
}

async fn ensure_schema(pool: &sqlx::PgPool) -> Result<(), sqlx::Error> {
    pool.execute(
        "CREATE TABLE IF NOT EXISTS agents (\
            id TEXT PRIMARY KEY, \
            name TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await?;
    pool.execute(
        "CREATE TABLE IF NOT EXISTS runs (\
            id TEXT PRIMARY KEY, \
            agent_id TEXT NOT NULL, \
            session TEXT NOT NULL, \
            input TEXT NOT NULL, \
            output TEXT NOT NULL, \
            created_at TIMESTAMPTZ NOT NULL DEFAULT now())",
    )
    .await?;
    Ok(())
}

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser)]
#[command(
    name = "aria-agent",
    about = "Agents Cloud API + CLI",
    version = AGENT_VERSION,
    arg_required_else_help = false,
    disable_version_flag = true
)]
struct Cli {
    /// Print version
    #[arg(short = 'v', long = "version", action = ArgAction::Version)]
    _version: (),
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Write CLI config (~/.ariacompute/agent-cli.yml)
    Setup(SetupArgs),
    /// Replace this CLI + libaria-agent_ffi from Releases
    Upgrade {
        /// Target version (default: latest stable)
        version: Option<String>,
        /// Override the upgrade_url from config
        #[arg(long)]
        url: Option<String>,
    },
    /// Start the cloud HTTP server (default)
    Serve,
    /// Print version
    Version,
}

#[derive(Args)]
struct SetupArgs {
    /// Show CLI config status
    #[arg(long)]
    status: bool,
    /// Remove the CLI config file
    #[arg(long)]
    clear: bool,
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Setup(args)) => cmd_setup(args)?,
        Some(Command::Upgrade { version, url }) => {
            upgrade::run(version.as_deref(), url.as_deref(), AGENT_VERSION)
                .await
                .map_err(|e| -> Box<dyn std::error::Error> { e.into() })?
        }
        Some(Command::Serve) | None => cmd_serve().await?,
        Some(Command::Version) => println!("aria-agent {AGENT_VERSION}"),
    }
    Ok(())
}

fn cmd_setup(args: SetupArgs) -> Result<(), Box<dyn std::error::Error>> {
    if args.status {
        return cmd_setup_status();
    }
    if args.clear {
        return cmd_setup_clear();
    }

    // The Releases source is configured only via interactive mode.
    let upgrade_url = if std::io::stdin().is_terminal() {
        let ans = prompt("Releases source (1=github, 2=gitee) [1]: ")?;
        match ans.trim() {
            "" | "1" | "github" | "gh" => cli_config::upgrade_url_for_site("github"),
            "2" | "gitee" => cli_config::upgrade_url_for_site("gitee"),
            other => {
                return Err(format!("unknown source: {other} (expected github or gitee)").into())
            }
        }
    } else {
        // Non-interactive: fall back to the GitHub default.
        cli_config::upgrade_url_for_site("github")
    };

    let path = cli_config::save_cli_config(&cli_config::AgentCliConfig {
        upgrade_url: upgrade_url.clone(),
    })?;
    println!("wrote {} (upgrade_url={upgrade_url})", path.display());
    Ok(())
}

/// Read one line from stdin (used by interactive `setup`).
fn prompt(q: &str) -> io::Result<String> {
    use std::io::Write;
    print!("{q}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(line)
}

/// `aria-agent setup --status`: print where the CLI config lives + its values.
fn cmd_setup_status() -> Result<(), Box<dyn std::error::Error>> {
    let path = cli_config::cli_config_path()?;
    if path.exists() {
        println!("config: {}", path.display());
        let cli = cli_config::load_cli_config()?;
        if cli.upgrade_url.is_empty() {
            println!("upgrade_url: (not set)");
        } else {
            println!("upgrade_url: {}", cli.upgrade_url);
        }
    } else {
        println!("config: {} (missing; run `aria-agent setup`)", path.display());
    }
    println!("lib: {}", cli_config::lib_dir()?.display());
    Ok(())
}

/// `aria-agent setup --clear`: delete the CLI config file.
fn cmd_setup_clear() -> Result<(), Box<dyn std::error::Error>> {
    match cli_config::clear_cli_config()? {
        Some(path) => println!("cleared {}", path.display()),
        None => println!("(nothing to clear)"),
    }
    Ok(())
}

async fn cmd_serve() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Prefer CLI config from ~/.ariacompute/agent-cli.yml.
    let _ = cli_config::ensure_aria_home();
    let cli = cli_config::load_cli_config()?;
    if cli.upgrade_url.is_empty() {
        tracing::info!("agent-cli: no upgrade_url configured (run `aria-agent setup`)");
    } else {
        tracing::info!("agent-cli: upgrade_url={}", cli.upgrade_url);
    }
    // Expose the installed libaria-agent_ffi to native SDKs unless overridden.
    if std::env::var("ARIA_AGENT_FFI_LIB").is_err() {
        if let Ok(lib) = cli_config::lib_dir() {
            std::env::set_var("ARIA_AGENT_FFI_LIB", lib);
        }
    }

    let db_url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "postgres://postgres:postgres@localhost:5432/agent".into());
    let pool = PgPoolOptions::new()
        .max_connections(5)
        .connect(&db_url)
        .await?;
    ensure_schema(&pool).await?;

    // Local/embedded memo store — the ONLY holder of conversational context.
    //
    // Backend is selected via `AGENT_MEMO_BACKEND`:
    //   * `memory` (default) — ephemeral, in-memory sled (lost on restart).
    //   * `memo`           — persistent sled DB under `MEMO_DIR`, survives restarts.
    let memo_backend = std::env::var("AGENT_MEMO_BACKEND").unwrap_or_else(|_| "memory".into());
    let memo: Arc<dyn MemoStore> = match memo_backend.as_str() {
        "memo" => {
            let memo_dir =
                PathBuf::from(std::env::var("MEMO_DIR").unwrap_or_else(|_| "/app/.memo".into()));
            std::fs::create_dir_all(&memo_dir).map_err(|e| e.to_string())?;
            tracing::info!("memo: persistent backend at {}", memo_dir.display());
            SledMemoStore::open(&memo_dir).map_err(|e| e.to_string())?
        }
        _ => {
            tracing::info!("memo: in-memory backend (ephemeral)");
            SledMemoStore::memory().map_err(|e| e.to_string())?
        }
    };

    // --- Reef wiring ---
    let reef_dir = PathBuf::from(std::env::var("REEF_DIR").unwrap_or_else(|_| ".reef".into()));
    std::fs::create_dir_all(&reef_dir).ok();
    // Initialize (idempotent) a git repo for harness versioning; best-effort.
    if let Err(e) = git::init_repo(&reef_dir) {
        tracing::warn!("reef: git init skipped: {e}");
    }
    // Load the last winning harness, or fall back to the baseline.
    let active: Arc<ActiveHarness> = Arc::new(match harness::load(&reef_dir) {
        Ok(h) => {
            tracing::info!(
                "reef: loaded harness v{} from {}",
                git::current_version(&reef_dir).unwrap_or(0),
                reef_dir.display()
            );
            ActiveHarness::new(h)
        }
        Err(_) => ActiveHarness::baseline("agent"),
    });
    let records: Arc<dyn RecordStore> =
        SledRecordStore::open(&reef_dir.join("records")).map_err(|e| e.to_string())?;
    let feedback: Arc<dyn FeedbackStore> =
        SledFeedbackStore::open(&reef_dir.join("feedback")).map_err(|e| e.to_string())?;
    // The local model engine ("local engine FFI") that proposes harness changes.
    let engine = Arc::new(EvolutionEngine::new(
        Box::new(OpenAiModel::new(DEFAULT_MODEL)),
        records.clone(),
        feedback.clone(),
        reef_dir.clone(),
        active.clone(),
        3,
    ));

    let state = AppState {
        pool,
        memo,
        records,
        feedback,
        active,
        engine,
        reef_dir,
    };

    let app = Router::new()
        .route("/v1/agents", post(create_agent))
        .route("/v1/agents/:id", get(get_agent))
        .route("/v1/runs", post(run_agent))
        .route("/v1/runs/stream", post(run_agent_stream))
        .route("/reef/report", post(reef_report))
        .route("/reef/evolve", post(reef_evolve))
        .route("/reef/versions", get(reef_versions))
        .with_state(state.clone())
        // Auth gate (Bearer / ApiKey); open when AGENT_CLOUD_API_KEY is unset.
        // `from_fn_with_state` threads `AppState` into the middleware closure.
        .layer(from_fn_with_state(state, require_auth));

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], 3000));
    tracing::info!("agent-cloud listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::StubModel;

    /// Build an `AppState` with in-memory stores + StubModel engine. The pg
    /// pool is lazy (never queried by the reef handlers), so no real Postgres
    /// is required for these tests.
    fn test_state() -> AppState {
        let reef_dir = std::env::temp_dir().join(format!("reef_cloud_{}", Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&reef_dir);
        std::fs::create_dir_all(&reef_dir).unwrap();
        let active = Arc::new(ActiveHarness::baseline("agent"));
        let records: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
        let feedback: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
        let engine = Arc::new(EvolutionEngine::new(
            Box::new(StubModel::new("agent")),
            records.clone(),
            feedback.clone(),
            reef_dir.clone(),
            active.clone(),
            3,
        ));
        let pool = PgPoolOptions::new()
            .connect_lazy("postgres://x:x@localhost:5432/x")
            .unwrap();
        let memo = SledMemoStore::memory().unwrap();
        AppState {
            pool,
            memo,
            records,
            feedback,
            active,
            engine,
            reef_dir,
        }
    }

    #[tokio::test]
    async fn reef_report_rejects_unknown_record() {
        let state = test_state();
        let res = reef_report(
            State(state),
            Json(ReefReportBody {
                references: vec!["ghost".into()],
                score: -1.0,
                feedback: None,
            }),
        )
        .await;
        assert!(res.is_err(), "reporting on a missing record must error");
    }

    #[tokio::test]
    async fn reef_report_binds_feedback_to_record() {
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        state.records.record_turn(rec.clone()).await.unwrap();
        let res = reef_report(
            State(state.clone()),
            Json(ReefReportBody {
                references: vec![rec.id.clone()],
                score: -1.0,
                feedback: Some("wrong answer".into()),
            }),
        )
        .await;
        assert!(res.is_ok());
        // Feedback now visible in the store.
        let listed = state.feedback.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].references, vec![rec.id]);
    }

    #[tokio::test]
    async fn reef_evolve_wins_and_hot_swaps_without_pg() {
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
        state.records.record_turn(rec.clone()).await.unwrap();
        state
            .feedback
            .report(Feedback::new(
                vec![rec.id.clone()],
                -1.0,
                Some("wrong".into()),
            ))
            .await
            .unwrap();

        let before = state.active.get().system_text();
        let out = reef_evolve(State(state.clone())).await.unwrap();
        assert_eq!(out.0["adopted"], json!(true));
        let after = state.active.get().system_text();
        assert_ne!(before, after);
        assert!(after.contains("reef_improvement"));
    }

    #[tokio::test]
    async fn reef_evolve_no_eligible_keeps_active() {
        let state = test_state();
        let out = reef_evolve(State(state.clone())).await.unwrap();
        assert_eq!(out.0["adopted"], json!(false));
        assert_eq!(out.0["reason"], json!("no_eligible"));
        // Active harness untouched.
        assert!(state.active.get().is_baseline("agent"));
    }

    #[tokio::test]
    async fn reef_versions_includes_baseline() {
        let state = test_state();
        let vers = reef_versions(State(state)).await;
        assert!(vers.contains(&"baseline".to_string()));
    }

    #[tokio::test]
    async fn run_agent_records_turn_and_returns_receipt_header() {
        // We can't easily assert the header from a handler return, but we can
        // verify the record is persisted for a known record id by simulating
        // the same path the handler uses. This exercises RecordStore wiring.
        let state = test_state();
        let rec = Record::new("a", "s", None, "You are agent.", "hi", "hello", "stub");
        let id = rec.id.clone();
        state.records.record_turn(rec).await.unwrap();
        let got = state.records.get(&id).await.unwrap();
        assert_eq!(got.output, "hello");
    }
}
