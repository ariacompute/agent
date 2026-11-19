//! Real-socket end-to-end tests for the OpenAI beta Agents HTTP surface.
//!
//! The production axum router is bound to an ephemeral loopback port and
//! driven with a real HTTP client, so routing, auth middleware, request
//! decoding, SSE encoding and the agent loop are all exercised together.
//!
//! Optional dependencies never fake a pass:
//! * `DATABASE_URL` (Postgres + pgvector) — the DB scenario skips with a reason
//!   when unset or unreachable, matching the workspace's DB-gated test rule.
//! * `ARIA_E2E_DOCKER=1` — the Docker scenario additionally skips when no
//!   daemon answers; it is opt-in because it starts a real container.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use agent_core::context::{ContextStore, MemoryContextStore};
use agent_core::seams::{now_ms, AuditEvent, AuditKind, MeterState, ResourceRequest};
use agent_core::{ModelClient, ModelRequest, ModelResponse, ModelTurn, ToolCall};
use agent_sandbox::{
    CheckpointRef, DockerSandbox, ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle,
};
use aria_agent_cloud::context::{ensure_pgvector, PgContextStore};
use aria_agent_cloud::governance::{global_store, usage_record};
use aria_agent_cloud::store::PgStore;
use aria_agent_cloud::{
    build_router, hash_key, ApiKeyStore, AppState, KeyCache, MakeModel, MakeSandbox, MemoryStore,
    NewApiKeyRow, Principal,
};
use axum::Router;
use reqwest::{Client as HttpClient, Method, StatusCode};
use serde_json::{json, Value};
use sqlx::postgres::{PgPool, PgPoolOptions};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// Owns the spawned server task; aborting it releases the port on drop.
struct Server {
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn serve(app: Router) -> (Api, Server) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback");
    let addr = listener.local_addr().expect("local addr");
    let task = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            eprintln!("e2e server stopped: {e}");
        }
    });
    (
        Api {
            http: HttpClient::new(),
            base: format!("http://{addr}"),
        },
        Server { task },
    )
}

struct Api {
    http: HttpClient,
    base: String,
}

impl Api {
    async fn call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, String) {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request reaches the e2e server");
        let status = resp.status();
        (status, resp.text().await.expect("response body"))
    }

    async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Value) {
        let (status, text) = self.call(method, path, token, body).await;
        let value = if text.is_empty() {
            Value::Null
        } else {
            serde_json::from_str(&text).unwrap_or(Value::Null)
        };
        (status, value)
    }
}

/// `AGENT_CLOUD_API_KEY` is read (never mutated) so the suite coexists with
/// both auth modes. In open mode requests are unauthenticated by default.
fn bootstrap_token() -> Option<String> {
    std::env::var("AGENT_CLOUD_API_KEY")
        .ok()
        .filter(|k| !k.trim().is_empty())
}

/// Issue a scoped key resolved through the real auth middleware. Governance
/// scope gates are only meaningful when the key travels over the socket.
async fn issue_key(h: &MemoryHarness, token: &str, principal_id: &str, scopes: &[&str]) {
    h.store
        .insert_key(&NewApiKeyRow {
            id: format!("key-{principal_id}"),
            key_prefix: token.chars().take(8).collect(),
            key_hash: hash_key(token),
            principal_id: principal_id.to_string(),
            label: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        })
        .await
        .expect("insert scoped key");
}

fn sse_frames(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

/// In-process sandbox: records the command rather than starting a container.
struct StubSandbox;

#[async_trait::async_trait]
impl Sandbox for StubSandbox {
    async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        Ok(SandboxHandle { id: "stub".into() })
    }

    async fn exec(
        &self,
        _handle: &SandboxHandle,
        cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        if cmd.is_empty() {
            return Err(SandboxError::Exec("empty command".into()));
        }
        Ok(ExecOutput {
            exit_code: 0,
            stdout: cmd.join(" "),
            stderr: String::new(),
        })
    }

    async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
        Ok(())
    }
}

/// One tool call followed by a reply, to exercise the tool pipeline over HTTP.
struct ToolCallModel;

#[async_trait::async_trait]
impl ModelClient for ToolCallModel {
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, agent_core::CoreError> {
        Ok(ModelResponse {
            text: format!("final: {}", req.input),
        })
    }

    async fn complete_with_tools(
        &self,
        req: &ModelRequest,
        _tools: &[agent_core::Tool],
    ) -> Result<ModelTurn, agent_core::CoreError> {
        if req.context.contains("TOOL_RESULT") {
            Ok(ModelTurn::reply("done after tool"))
        } else {
            Ok(ModelTurn {
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_e2e".into(),
                    name: "shell".into(),
                    arguments: json!({"command": ["echo", "hi"]}),
                }],
                usage: None,
            })
        }
    }
}

/// Shared per-principal context stores, like the tenant-sharded production DB.
#[derive(Clone, Default)]
struct Contexts(Arc<Mutex<HashMap<String, Arc<MemoryContextStore>>>>);

impl Contexts {
    fn store_for(&self, principal: &Principal) -> Arc<dyn ContextStore> {
        let mut guard = self.0.lock().expect("context mutex");
        guard.entry(principal.id.clone()).or_default().clone() as Arc<dyn ContextStore>
    }
}

struct MemoryHarness {
    api: Api,
    store: Arc<MemoryStore>,
    _server: Server,
}

async fn memory_harness(model: MakeModel, sandbox: MakeSandbox) -> MemoryHarness {
    let store = Arc::new(MemoryStore::new());
    let contexts = Contexts::default();
    let ctx = contexts.clone();
    let make_context = Arc::new(move |p: &Principal| ctx.store_for(p));
    let state = AppState {
        agents: store.clone(),
        sessions: store.clone(),
        keys: store.clone(),
        teams: Arc::new(aria_agent_orchestrator::MemoryTeams::new()),
        schedule: Arc::new(aria_agent_orchestrator::MemorySchedule::new()),
        titles: store.clone(),
        key_cache: KeyCache::new(std::time::Duration::from_secs(60)),
        make_model: model,
        make_sandbox: sandbox,
        make_context,
        harness: None,
    };
    let (api, server) = serve(build_router(state)).await;
    MemoryHarness {
        api,
        store,
        _server: server,
    }
}

fn stub_model() -> MakeModel {
    Arc::new(|| Box::new(agent_core::StubModel::new("e2e")) as Box<dyn ModelClient>)
}

fn stub_sandbox() -> MakeSandbox {
    Arc::new(|| Arc::new(StubSandbox) as Arc<dyn Sandbox>)
}

#[tokio::test]
async fn beta_agents_lifecycle_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    let (status, body) = h.api.call(Method::GET, "/healthz", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body, "ok\n");

    let (status, listed) = h.api.send(Method::GET, "/v1/agents", auth, None).await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert_eq!(listed["object"], "list");

    // Abnormal: the health probe stays open, the API honours the auth gate.
    let (unauth, _) = h.api.send(Method::GET, "/v1/agents", None, None).await;
    assert_eq!(
        unauth,
        if auth.is_some() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        }
    );

    let name = format!("e2e-{}", uuid::Uuid::new_v4());
    let (status, created) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let agent_id = created["id"].as_str().expect("agent id").to_string();

    let (status, fetched) = h
        .api
        .send(Method::GET, &format!("/v1/agents/{agent_id}"), auth, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{fetched}");
    assert_eq!(fetched["name"], json!(name));
    let (_, listed) = h.api.send(Method::GET, "/v1/agents", auth, None).await;
    assert!(listed["data"]
        .as_array()
        .expect("agents")
        .iter()
        .any(|a| a["id"] == json!(agent_id)));

    let (status, session) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions",
            auth,
            Some(json!({"agent_id": agent_id})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["id"].as_str().expect("session id").to_string();

    // Blocking turn + persisted items/turns/title.
    let (status, turn) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events"),
            auth,
            Some(json!({"input": "e2e hello"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{turn}");
    assert_eq!(turn["status"], "completed");
    assert!(turn["output"]
        .as_str()
        .expect("output")
        .contains("e2e hello"));

    let (_, items) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/items"),
            auth,
            None,
        )
        .await;
    let items = items["data"].as_array().expect("items");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[1]["role"], "assistant");

    let (_, turns) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/turns"),
            auth,
            None,
        )
        .await;
    let turns = turns["data"].as_array().expect("turns");
    assert_eq!(turns.len(), 1);
    let turn_id = turns[0]["id"].as_str().expect("turn id");
    let (status, one) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/turns/{turn_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{one}");
    assert_eq!(one["id"], json!(turn_id));

    let (status, title) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/title"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{title}");
    assert_eq!(title["source"], "first_prompt");

    // Long-term memory is written and recalled through the same HTTP surface.
    let memory = format!("/v1/agents/sessions/{sid}/memory");
    let (status, written) = h
        .api
        .send(
            Method::POST,
            &memory,
            auth,
            Some(json!({"key": "e2e-fact", "value": "the sea is blue", "kind": "note"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    let (status, read) = h
        .api
        .send(Method::GET, &format!("{memory}/e2e-fact"), auth, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["content"], "the sea is blue");
    let (status, recalled) = h
        .api
        .send(
            Method::GET,
            &format!("{memory}?text=sea&top_k=5"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{recalled}");
    assert!(recalled["data"]
        .as_array()
        .expect("recall")
        .iter()
        .any(|m| m["content"] == "the sea is blue"));

    // SSE contract: created first, terminal last, monotonic sequence, no sentinel.
    let (status, sse) = h
        .api
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events/stream"),
            auth,
            Some(json!({"input": "stream hello"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sse}");
    assert!(!sse.contains("[DONE]"), "{sse}");
    let frames = sse_frames(&sse);
    assert!(!frames.is_empty(), "{sse}");
    assert_eq!(frames[0]["type"], "agent.turn.created");
    assert_eq!(
        frames.last().expect("frames")["type"],
        "agent.turn.completed"
    );
    let seqs: Vec<u64> = frames
        .iter()
        .map(|f| f["sequence_number"].as_u64().expect("sequence_number"))
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, seqs, "sequence numbers must be monotonic");
    assert!(frames.iter().any(|f| {
        f["type"] == "agent.turn.output_text.delta"
            && f["delta"]
                .as_str()
                .is_some_and(|d| d.contains("stream hello"))
    }));
    for f in &frames {
        assert_eq!(f["session_id"], json!(sid));
    }

    let (status, after) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/items"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert!(after["data"].as_array().expect("items").len() > 2);

    // Abnormal paths: empty input, unknown session, invalid agent, unimplemented.
    let (empty, _) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events"),
            auth,
            Some(json!({"input": "   "})),
        )
        .await;
    assert_eq!(empty, StatusCode::BAD_REQUEST);
    let (missing, _) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions/nope/events",
            auth,
            Some(json!({"input": "hi"})),
        )
        .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);
    let (invalid, _) = h
        .api
        .send(Method::POST, "/v1/agents/sessions", auth, Some(json!({})))
        .await;
    assert_eq!(invalid, StatusCode::BAD_REQUEST);
    let (stub, _) = h.api.send(Method::GET, "/v1/vaults", auth, None).await;
    assert_eq!(stub, StatusCode::NOT_IMPLEMENTED);

    let (deleted, _) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/agents/sessions/{sid}"),
            auth,
            None,
        )
        .await;
    assert_eq!(deleted, StatusCode::OK);
    let (gone, _) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/items"),
            auth,
            None,
        )
        .await;
    assert_eq!(gone, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn tool_calls_stream_and_persist_over_a_real_socket() {
    let h = memory_harness(
        Arc::new(|| Box::new(ToolCallModel) as Box<dyn ModelClient>),
        stub_sandbox(),
    )
    .await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let (_, agent) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": format!("tool-{}", uuid::Uuid::new_v4())})),
        )
        .await;
    let (_, session) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions",
            auth,
            Some(json!({"agent_id": agent["id"]})),
        )
        .await;
    let sid = session["id"].as_str().expect("session id");
    let (status, sse) = h
        .api
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events/stream"),
            auth,
            Some(json!({"input": "run a command"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sse}");
    let frames = sse_frames(&sse);
    assert!(frames
        .iter()
        .any(|f| f["type"] == "agent.turn.item.added" && f["item"]["name"] == "shell"));
    let done = frames
        .iter()
        .find(|f| f["type"] == "agent.turn.item.done" && f["item"]["name"] == "shell")
        .expect("tool item.done frame");
    assert_eq!(done["item"]["status"], "completed");
    assert!(done["item"]["result"]["content"]
        .as_str()
        .expect("tool result")
        .contains("echo hi"));
    assert_eq!(
        frames.last().expect("frames")["type"],
        "agent.turn.completed"
    );

    let (_, items) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/items"),
            auth,
            None,
        )
        .await;
    assert!(items["data"]
        .as_array()
        .expect("items")
        .iter()
        .any(|i| i["content"].to_string().contains("done after tool")));
}

#[tokio::test]
async fn tenants_cannot_cross_read_sessions_over_http() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let tag = uuid::Uuid::new_v4();
    let tokens = [format!("e2e-a-{tag}"), format!("e2e-b-{tag}")];
    for (i, token) in tokens.iter().enumerate() {
        h.store
            .insert_key(&NewApiKeyRow {
                id: format!("key-{i}-{tag}"),
                key_prefix: token.chars().take(8).collect(),
                key_hash: hash_key(token),
                principal_id: format!("tenant-{i}-{tag}"),
                label: None,
                scopes: vec!["read".into(), "write".into()],
            })
            .await
            .expect("insert tenant key");
    }
    let (a, b) = (tokens[0].as_str(), tokens[1].as_str());

    let (status, agent) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            Some(a),
            Some(json!({"name": format!("a-{tag}")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let (status, session) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions",
            Some(a),
            Some(json!({"agent_id": agent["id"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["id"].as_str().expect("session id");
    let (status, _) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events"),
            Some(a),
            Some(json!({"input": "private note"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    let (status, b_agents) = h.api.send(Method::GET, "/v1/agents", Some(b), None).await;
    assert_eq!(status, StatusCode::OK, "{b_agents}");
    assert!(b_agents["data"].as_array().expect("agents").is_empty());
    for (method, path, body) in [
        (Method::GET, format!("/v1/agents/sessions/{sid}"), None),
        (
            Method::GET,
            format!("/v1/agents/sessions/{sid}/items"),
            None,
        ),
        (
            Method::POST,
            format!("/v1/agents/sessions/{sid}/events"),
            Some(json!({"input": "intrusion"})),
        ),
    ] {
        let (status, body) = h.api.send(method, &path, Some(b), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
    let (status, own) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/items"),
            Some(a),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{own}");
    assert!(own["data"].as_array().expect("items").len() >= 2);
}

#[tokio::test]
async fn postgres_backed_routes_persist_across_servers() {
    let url = match std::env::var("DATABASE_URL") {
        Ok(u) if !u.trim().is_empty() => u,
        _ => {
            eprintln!("skip: DATABASE_URL not set");
            return;
        }
    };
    let pool = match PgPoolOptions::new().max_connections(5).connect(&url).await {
        Ok(p) => p,
        Err(e) => {
            eprintln!("skip: DATABASE_URL unreachable: {e}");
            return;
        }
    };
    if let Err(e) = ensure_pgvector(&pool).await {
        eprintln!("skip: pgvector not available: {e}");
        return;
    }
    aria_agent_cloud::store::ensure_schema(&pool)
        .await
        .expect("ensure cloud schema");
    aria_agent_cloud::context::pg::ensure_schema(&pool)
        .await
        .expect("ensure context schema");

    let state = |pool: PgPool| {
        let pg = Arc::new(PgStore::new(pool.clone()));
        let teams: Arc<dyn aria_agent_orchestrator::teams::TeamStore> = pg.clone();
        let schedule: Arc<dyn agent_core::seams::ScheduleSeam> = pg.clone();
        let titles: Arc<dyn aria_agent_cloud::store::TitleStore> = pg.clone();
        let make_context: aria_agent_cloud::MakeContext = Arc::new(move |p: &Principal| {
            PgContextStore::new(pool.clone(), p.id.clone()) as Arc<dyn ContextStore>
        });
        AppState {
            agents: pg.clone(),
            sessions: pg.clone(),
            keys: pg.clone(),
            teams,
            schedule,
            titles,
            key_cache: KeyCache::new(std::time::Duration::from_secs(60)),
            make_model: stub_model(),
            make_sandbox: stub_sandbox(),
            make_context,
            harness: None,
        }
    };

    let (first, _server) = serve(build_router(state(pool.clone()))).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();
    let (status, agent) = first
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": format!("pg-{tag}")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let (status, session) = first
        .send(
            Method::POST,
            "/v1/agents/sessions",
            auth,
            Some(json!({"agent_id": agent["id"]})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["id"].as_str().expect("session id");
    let (status, _) = first
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events"),
            auth,
            Some(json!({"input": "postgres turn"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = first
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/memory"),
            auth,
            Some(json!({"key": format!("pg-{tag}"), "value": "persisted in pgvector"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK);

    // A brand-new router over the same pool sees the committed rows: nothing
    // lives in a process-local store.
    let (second, _server2) = serve(build_router(state(pool))).await;
    let (status, read) = second
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/memory/pg-{tag}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(read["content"], "persisted in pgvector");
    let (status, turns) = second
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/turns"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{turns}");
    assert_eq!(turns["data"].as_array().expect("turns").len(), 1);
}

#[tokio::test]
async fn docker_sandbox_executes_a_tool_from_the_stream_route() {
    if std::env::var("ARIA_E2E_DOCKER").ok().as_deref() != Some("1") {
        eprintln!("skip: set ARIA_E2E_DOCKER=1 to run the Docker E2E scenario");
        return;
    }
    let probe = DockerSandbox::new();
    let docker = match probe.client().await {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip: docker unavailable: {e}");
            return;
        }
    };
    if let Err(e) = docker.ping().await {
        eprintln!("skip: docker daemon unreachable: {e}");
        return;
    }
    // The default shell image must already be cached: E2E never pulls from the
    // network, so a missing image is a skipped dependency, not a failed test.
    if let Err(e) = docker.inspect_image("alpine:latest").await {
        eprintln!("skip: alpine:latest is not cached locally: {e}");
        return;
    }

    let make_sandbox: MakeSandbox = Arc::new(|| Arc::new(DockerSandbox::new()) as Arc<dyn Sandbox>);
    let h = memory_harness(
        Arc::new(|| Box::new(ToolCallModel) as Box<dyn ModelClient>),
        make_sandbox,
    )
    .await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let (_, agent) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": format!("docker-{}", uuid::Uuid::new_v4())})),
        )
        .await;
    let (_, session) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions",
            auth,
            Some(json!({"agent_id": agent["id"]})),
        )
        .await;
    let sid = session["id"].as_str().expect("session id");
    let (status, sse) = h
        .api
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events/stream"),
            auth,
            Some(json!({"input": "echo from docker"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sse}");
    let frames = sse_frames(&sse);
    let done = frames
        .iter()
        .find(|f| f["type"] == "agent.turn.item.done" && f["item"]["name"] == "shell")
        .expect("docker tool item.done frame");
    assert_eq!(done["item"]["result"]["is_error"], false, "{done}");
    assert!(done["item"]["result"]["content"]
        .as_str()
        .expect("docker tool output")
        .contains("hi"));
    assert_eq!(
        frames.last().expect("frames")["type"],
        "agent.turn.completed"
    );
}

#[tokio::test]
async fn governance_policy_revisions_and_scopes_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let tag = uuid::Uuid::new_v4();
    let (rw, ro, wo, blind) = (
        format!("gov-rw-{tag}"),
        format!("gov-ro-{tag}"),
        format!("gov-wo-{tag}"),
        format!("gov-blind-{tag}"),
    );
    issue_key(&h, &rw, &format!("gov-a-{tag}"), &["read", "write"]).await;
    issue_key(&h, &ro, &format!("gov-b-{tag}"), &["read"]).await;
    issue_key(&h, &wo, &format!("gov-c-{tag}"), &["write"]).await;
    issue_key(&h, &blind, &format!("gov-d-{tag}"), &[]).await;
    let path = "/v1/governance/egress-policy";

    let (status, policy) = h.api.send(Method::GET, path, Some(&rw), None).await;
    assert_eq!(status, StatusCode::OK, "{policy}");
    assert_eq!(policy["mode"], "deny_all");
    assert_eq!(policy["revision"], 1);

    // Scope gates are enforced by the auth middleware before the handler runs.
    for token in [&blind, &wo] {
        let (status, _) = h.api.send(Method::GET, path, Some(token), None).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }
    let (status, _) = h
        .api
        .send(
            Method::PUT,
            path,
            Some(&ro),
            Some(json!({"mode": "deny_all", "expected_revision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A write-only key can mutate its own policy but cannot read it back.
    let (status, written) = h
        .api
        .send(
            Method::PUT,
            path,
            Some(&wo),
            Some(json!({"mode": "deny_all", "expected_revision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["revision"], 2);

    let (status, written) = h
        .api
        .send(
            Method::PUT,
            path,
            Some(&rw),
            Some(json!({
                "mode": "allowlist",
                "allowed": ["api.example.com"],
                "expected_revision": 1,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["revision"], 2);
    assert_eq!(written["allowed"][0], "api.example.com");

    // A stale write loses with 409 and never overwrites the winner.
    let (status, conflict) = h
        .api
        .send(
            Method::PUT,
            path,
            Some(&rw),
            Some(json!({"mode": "deny_all", "expected_revision": 1})),
        )
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{conflict}");
    let (_, unchanged) = h.api.send(Method::GET, path, Some(&rw), None).await;
    assert_eq!(unchanged["revision"], 2);

    // PATCH is revision guarded as well.
    let (status, patched) = h
        .api
        .send(
            Method::PATCH,
            path,
            Some(&rw),
            Some(json!({
                "expected_revision": 2,
                "allow_add": ["*.pypi.org"],
                "allow_remove": ["api.example.com"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{patched}");
    assert_eq!(patched["revision"], 3);
    assert_eq!(patched["allowed"], json!(["*.pypi.org"]));

    // Malformed policies are client errors, never 500s.
    for body in [
        json!({"mode": "open", "expected_revision": 3}),
        json!({"mode": "allowlist", "allowed": ["https://x"], "expected_revision": 3}),
    ] {
        let (status, _) = h.api.send(Method::PUT, path, Some(&rw), Some(body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    // Tenant isolation: the read-only principal still sees its own deny_all.
    let (status, other) = h.api.send(Method::GET, path, Some(&ro), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(other["revision"], 1);
    assert_eq!(other["mode"], "deny_all");
}

#[tokio::test]
async fn governance_audit_usage_and_status_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let tag = uuid::Uuid::new_v4();
    let key = format!("gov-read-{tag}");
    let pid = format!("gov-audit-{tag}");
    let other_pid = format!("gov-other-{tag}");
    issue_key(&h, &key, &pid, &["read"]).await;

    let store = global_store();
    store
        .append_audit(AuditEvent::new(AuditKind::EgressDecision, &pid, "a.com").with_decision(true))
        .await
        .expect("append audit");
    store
        .append_audit(AuditEvent::new(AuditKind::ToolExecuted, &pid, "shell"))
        .await
        .expect("append audit");
    store
        .append_audit(AuditEvent::new(
            AuditKind::EgressDecision,
            &other_pid,
            "b.com",
        ))
        .await
        .expect("append other tenant audit");
    let shape = ResourceRequest {
        vcpus_x1000: 1000,
        memory_mib: 1024,
        disk_mib: 0,
    };
    store
        .record_usage(usage_record(
            &pid,
            "sbx",
            MeterState::Running,
            shape,
            0,
            3_600_000,
        ))
        .await
        .expect("record running usage");
    store
        .record_usage(usage_record(
            &pid,
            "sbx",
            MeterState::Paused,
            shape,
            3_600_000,
            7_200_000,
        ))
        .await
        .expect("record paused usage");

    let (status, audit) = h
        .api
        .send(Method::GET, "/v1/governance/audit", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{audit}");
    let events = audit.as_array().expect("audit array");
    assert_eq!(events.len(), 2, "other tenants do not leak: {audit}");
    assert!(events.iter().all(|e| e["principal_id"] == json!(pid)));

    let (status, filtered) = h
        .api
        .send(
            Method::GET,
            "/v1/governance/audit?kind=tool_executed",
            Some(&key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(filtered.as_array().expect("filtered audit").len(), 1);
    let (status, _) = h
        .api
        .send(
            Method::GET,
            "/v1/governance/audit?kind=nope",
            Some(&key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let (status, ndjson) = h
        .api
        .call(Method::GET, "/v1/governance/audit/export", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{ndjson}");
    assert_eq!(ndjson.lines().count(), 2, "{ndjson}");
    assert!(ndjson.lines().all(|l| l.starts_with('{')), "{ndjson}");

    let (status, usage) = h
        .api
        .send(Method::GET, "/v1/governance/usage", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{usage}");
    assert_eq!(usage["records"], 2);
    assert_eq!(usage["resources"]["vcpu_millis"], 3_600_000);
    assert_eq!(
        usage["resources"]["memory_mib_millis"],
        json!(2u64 * 1024 * 3_600_000)
    );

    let (status, reported) = h
        .api
        .send(Method::GET, "/v1/governance/status", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{reported}");
    let flag = |name: &str, default: bool| {
        std::env::var(name)
            .ok()
            .map(|v| !v.trim().eq_ignore_ascii_case("off"))
            .unwrap_or(default)
    };
    assert_eq!(
        reported["enforcement"],
        json!(std::env::var("AGENT_EGRESS_ENFORCEMENT").unwrap_or_else(|_| "proxy".into()))
    );
    assert_eq!(
        reported["compression"],
        json!(flag("AGENT_COMPRESSION", true))
    );
    assert_eq!(reported["metering"], json!(flag("AGENT_METERING", true)));
    assert!(reported["credentials_configured"].is_boolean());
    assert!(reported["sandbox_configured"].is_boolean());
}

#[tokio::test]
async fn governance_credentials_and_checkpoints_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let tag = uuid::Uuid::new_v4();
    let key = format!("gov-manage-{tag}");
    let pid = format!("gov-checkpoint-{tag}");
    issue_key(&h, &key, &pid, &["read", "write"]).await;

    // `build_router` does not attach a credential store: the endpoints must
    // report the missing dependency loudly rather than serve fake metadata.
    for (method, path, body) in [
        (Method::GET, "/v1/governance/credentials", None),
        (
            Method::PUT,
            "/v1/governance/credentials/openai",
            Some(json!({"value": "sk-e2e"})),
        ),
        (Method::DELETE, "/v1/governance/credentials/openai", None),
    ] {
        let (status, _) = h.api.send(method, path, Some(&key), body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }

    let checkpoint = CheckpointRef::new("img:latest", "sbx", now_ms());
    global_store()
        .put_checkpoint(&pid, checkpoint.clone())
        .await
        .expect("seed checkpoint");
    let (status, listed) = h
        .api
        .send(Method::GET, "/v1/governance/checkpoints", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed
        .as_array()
        .expect("checkpoint list")
        .iter()
        .any(|c| c["id"] == json!(checkpoint.id)));

    // Creating a checkpoint needs a sandbox backend, which the test router
    // deliberately does not attach.
    let (status, _) = h
        .api
        .send(
            Method::POST,
            "/v1/governance/checkpoints",
            Some(&key),
            Some(json!({"sandbox_id": "sbx"})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    // Restoring distinguishes a missing checkpoint from a missing backend.
    let (status, _) = h
        .api
        .send(
            Method::POST,
            "/v1/governance/checkpoints/nope/restore",
            Some(&key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/governance/checkpoints/{}/restore", checkpoint.id),
            Some(&key),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_and_session_crud_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();
    let name = format!("crud-{tag}");

    let (status, agent) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": name})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let aid = agent["id"].as_str().expect("agent id");

    let (status, updated) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/{aid}"),
            auth,
            Some(json!({"name": format!("{name}-renamed")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["name"], json!(format!("{name}-renamed")));

    let (status, session) = h
        .api
        .send(
            Method::POST,
            "/v1/agents/sessions",
            auth,
            Some(json!({"agent_id": aid})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{session}");
    let sid = session["id"].as_str().expect("session id").to_string();

    let (status, sessions) = h
        .api
        .send(Method::GET, "/v1/agents/sessions", auth, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{sessions}");
    assert!(sessions["data"]
        .as_array()
        .expect("sessions")
        .iter()
        .any(|s| s["id"] == json!(sid)));

    let (status, updated) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}"),
            auth,
            Some(json!({"instructions": "be brief"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["instructions"], json!("be brief"));

    let (status, _) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/agents/sessions/{sid}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, _) = h
        .api
        .send(Method::DELETE, &format!("/v1/agents/{aid}"), auth, None)
        .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h
        .api
        .send(Method::GET, &format!("/v1/agents/{aid}"), auth, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}
