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
use agent_core::{ModelClient, ModelRequest, ModelResponse, ModelTurn, ToolCall};
use agent_sandbox::{DockerSandbox, ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use aria_agent_cloud::context::{ensure_pgvector, PgContextStore};
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
