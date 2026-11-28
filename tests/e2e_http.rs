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
    agent_from_request, build_router, hash_key, AgentStore, ApiKeyStore, AppState, KeyCache,
    MakeModel, MakeSandbox, MemoryStore, NewApiKeyRow, Principal,
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
        let (status, _, text) = self.call_with_type(method, path, token, body).await;
        (status, text)
    }

    /// Like `call`, but keeps the response `content-type`: an SSE route must be
    /// distinguishable from a JSON error body, and the socket is the only place
    /// that framing is produced.
    async fn call_with_type(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, Option<String>, String) {
        let mut req = self.http.request(method, format!("{}{}", self.base, path));
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let resp = req.send().await.expect("request reaches the e2e server");
        let status = resp.status();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        (
            status,
            content_type,
            resp.text().await.expect("response body"),
        )
    }

    /// POST a hand-written body, so a malformed envelope can be sent verbatim.
    async fn post_raw(&self, path: &str, token: Option<&str>, body: &str) -> (StatusCode, Value) {
        let mut req = self
            .http
            .post(format!("{}{}", self.base, path))
            .body(body.to_string());
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.expect("request reaches the e2e server");
        let status = resp.status();
        let text = resp.text().await.expect("response body");
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
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

/// Blocks inside the model call until the test releases it, so a cancellation
/// is guaranteed to land while the task is still working.
struct GatedModel(Arc<tokio::sync::Notify>);

#[async_trait::async_trait]
impl ModelClient for GatedModel {
    async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, agent_core::CoreError> {
        self.0.notified().await;
        Ok(ModelResponse {
            text: "released".into(),
        })
    }

    async fn complete_with_tools(
        &self,
        _req: &ModelRequest,
        _tools: &[agent_core::Tool],
    ) -> Result<ModelTurn, agent_core::CoreError> {
        self.0.notified().await;
        Ok(ModelTurn::reply("released"))
    }
}

/// Fails every completion, so the socket path must end the stream itself.
struct FailingModel;

#[async_trait::async_trait]
impl ModelClient for FailingModel {
    async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, agent_core::CoreError> {
        Err(agent_core::CoreError::Model("gateway down".into()))
    }

    async fn complete_with_tools(
        &self,
        _req: &ModelRequest,
        _tools: &[agent_core::Tool],
    ) -> Result<ModelTurn, agent_core::CoreError> {
        Err(agent_core::CoreError::Model("gateway down".into()))
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
    memory_harness_with(model, sandbox, None).await
}

/// The default agent the A2A surface serves on `/a2a`.
const A2A_AGENT_ID: &str = "a2a-default";
const A2A_AGENT_NAME: &str = "e2e-a2a";

/// The principal the seeded agent belongs to: the bootstrap admin when a key is
/// configured, the open-mode tenant otherwise.
fn a2a_owner() -> Principal {
    if bootstrap_token().is_some() {
        Principal::admin()
    } else {
        Principal::default_user()
    }
}

/// The same harness with the A2A surface on and one seeded default agent.
async fn a2a_harness(model: MakeModel, sandbox: MakeSandbox) -> MemoryHarness {
    memory_harness_with(model, sandbox, Some(A2A_AGENT_ID)).await
}

/// `default_agent`: when set, the A2A surface is enabled and that agent is
/// seeded for the principal the caller resolves to. Cards are public and the
/// card lookup uses the admin, but every JSON-RPC call resolves the agent as
/// the caller — so the row has to belong to that tenant.
async fn memory_harness_with(
    model: MakeModel,
    sandbox: MakeSandbox,
    default_agent: Option<&str>,
) -> MemoryHarness {
    let store = Arc::new(MemoryStore::new());
    let mut a2a = aria_agent_cloud::A2aState::disabled();
    if let Some(agent_id) = default_agent {
        let agent = agent_from_request(
            agent_id.to_string(),
            aria_agent_cloud::types::CreateAgentRequest {
                name: Some(A2A_AGENT_NAME.into()),
                ..Default::default()
            },
            1_700_000_000_000,
        )
        .expect("the seeded A2A agent is well formed");
        store
            .insert_agent(&a2a_owner(), &agent)
            .await
            .expect("seed the A2A agent");
        a2a = aria_agent_cloud::A2aState {
            enabled: true,
            config: Arc::new(aria_agent_cloud::A2aConfig {
                default_agent: Some(agent_id.to_string()),
                public_url: "https://cloud.example.test/".into(),
                ..Default::default()
            }),
            ..aria_agent_cloud::A2aState::disabled()
        };
    }
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
        rsi: aria_agent_cloud::RsiState::disabled(),
        a2a,
        cancel_harness: None,
    };
    let (api, server) = serve(build_router(state)).await;
    MemoryHarness {
        api,
        store,
        _server: server,
    }
}

/// One A2A user message, carrying both part discriminators like agenthub does.
fn a2a_msg(text: &str) -> Value {
    json!({
        "kind": "message",
        "role": "user",
        "messageId": "m-e2e",
        "parts": [{"kind": "text", "type": "text", "text": text}],
    })
}

/// A JSON-RPC envelope.
fn rpc_call(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// The `error.code` of a method-level failure (they travel inside a 200).
fn rpc_error(body: &Value) -> i64 {
    body["error"]["code"].as_i64().unwrap_or_default()
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
async fn a_model_failure_ends_the_stream_with_a_terminal_frame_over_a_real_socket() {
    let h = memory_harness(
        Arc::new(|| Box::new(FailingModel) as Box<dyn ModelClient>),
        stub_sandbox(),
    )
    .await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();
    let (_aid, sid) = agent_and_session(&h, auth, &tag).await;

    let (status, sse) = h
        .api
        .call(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events/stream"),
            auth,
            Some(json!({"input": "boom"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{sse}");
    let frames = sse_frames(&sse);
    let failed = frames.last().expect("at least one frame");
    assert_eq!(failed["type"], "agent.turn.failed", "{sse}");
    assert_eq!(failed["session_id"], json!(sid), "{sse}");
    assert!(
        failed["sequence_number"].as_u64().unwrap_or_default() >= 1,
        "{sse}"
    );

    // The failure is persisted, not just streamed: the turn is `failed` on a
    // later read.
    let (status, turns) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/turns"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{turns}");
    assert_eq!(turns["data"][0]["status"], "failed", "{turns}");
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
            rsi: aria_agent_cloud::RsiState::disabled(),
            a2a: aria_agent_cloud::A2aState::disabled(),
            cancel_harness: None,
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

    // The team and the scheduled job are Postgres-backed too: create them on
    // one router, prove them on the next.
    let (status, team) = first
        .send(
            Method::POST,
            "/v1/agent-teams",
            auth,
            Some(json!({
                "name": format!("pg-team-{tag}"),
                "tasks": [{"id": "t1", "title": "build"}],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{team}");
    let team_id = team["id"].as_str().expect("team id").to_string();
    let (status, job) = first
        .send(
            Method::POST,
            "/v1/scheduled-jobs",
            auth,
            Some(json!({
                "name": format!("pg-job-{tag}"),
                "every_secs": 60,
                "payload": {"kind": "sweep", "tag": tag.to_string()},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    let job_id = job["id"].as_str().expect("job id").to_string();

    // A brand-new router over the same pool sees the committed rows: nothing
    // lives in a process-local store.
    let (second, _server2) = serve(build_router(state(pool.clone()))).await;
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

    let (status, read_team) = second
        .send(
            Method::GET,
            &format!("/v1/agent-teams/{team_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{read_team}");
    assert_eq!(read_team["tasks"][0]["state"], "pending");
    let (status, after_claim) = second
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t1/claim"),
            auth,
            Some(json!({"worker": "pg-worker"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{after_claim}");
    assert_eq!(after_claim["tasks"][0]["claimed_by"], "pg-worker");
    // The claim is written, not just echoed: a third router reads it back.
    let (third, _server3) = serve(build_router(state(pool))).await;
    let (status, persisted) = third
        .send(
            Method::GET,
            &format!("/v1/agent-teams/{team_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{persisted}");
    assert_eq!(persisted["tasks"][0]["claimed_by"], "pg-worker");

    let (status, jobs) = second
        .send(Method::GET, "/v1/scheduled-jobs", auth, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{jobs}");
    let stored = jobs["data"]
        .as_array()
        .expect("jobs")
        .iter()
        .find(|j| j["id"] == json!(job_id))
        .unwrap_or_else(|| panic!("job {job_id} missing from {jobs}"));
    assert_eq!(stored["payload"]["tag"], json!(tag.to_string()));
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

/// One agent + session over the socket, for the endpoint families that hang off
/// a session.
async fn agent_and_session(
    h: &MemoryHarness,
    auth: Option<&str>,
    tag: &uuid::Uuid,
) -> (String, String) {
    let (status, agent) = h
        .api
        .send(
            Method::POST,
            "/v1/agents",
            auth,
            Some(json!({"name": format!("e2e-{tag}")})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{agent}");
    let aid = agent["id"].as_str().expect("agent id").to_string();
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
    (aid, session["id"].as_str().expect("session id").to_string())
}

#[tokio::test]
async fn api_keys_are_issued_used_and_revoked_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let tag = uuid::Uuid::new_v4();
    let admin = format!("e2e-admin-{tag}");
    issue_key(
        &h,
        &admin,
        &format!("admin-{tag}"),
        &["read", "write", "admin"],
    )
    .await;

    // A key created through HTTP must work on the very next request: the
    // plaintext is returned once, only its hash is stored.
    let (status, created) = h
        .api
        .send(
            Method::POST,
            "/v1/api-keys",
            Some(&admin),
            Some(json!({
                "label": "ci",
                "principal_id": format!("pid-{tag}"),
                "scopes": ["read"],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let key = created["key"]
        .as_str()
        .expect("plaintext key is returned once")
        .to_string();
    let id = created["id"].as_str().expect("key id").to_string();
    assert!(key.starts_with(created["key_prefix"].as_str().expect("prefix")));
    assert_eq!(created["principal_id"], json!(format!("pid-{tag}")));

    let (status, agents) = h
        .api
        .send(Method::GET, "/v1/agents", Some(&key), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{agents}");

    // Revocation purges the resolution cache, so the next request is already
    // rejected — not only after the cache entry would have expired.
    let (status, revoked) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/api-keys/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{revoked}");
    assert_eq!(revoked["ok"], json!(true));
    for _ in 0..2 {
        let (status, body) = h
            .api
            .send(Method::GET, "/v1/agents", Some(&key), None)
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    }

    // The revoked key is gone from the listing, and revoking twice is 404.
    let (status, listed) = h
        .api
        .send(Method::GET, "/v1/api-keys", Some(&admin), None)
        .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    // The key listing is a plain array (the only endpoint that is not a
    // `ListResponse`), so read it as one.
    assert!(!listed
        .as_array()
        .expect("key list")
        .iter()
        .any(|k| k["id"] == json!(id)));
    let (status, _) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/api-keys/{id}"),
            Some(&admin),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn team_task_state_machine_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();

    let (status, team) = h
        .api
        .send(
            Method::POST,
            "/v1/agent-teams",
            auth,
            Some(json!({
                "name": format!("release-{tag}"),
                "tasks": [
                    {"id": "t1", "title": "build"},
                    {"id": "t2", "title": "ship", "depends_on": ["t1"]},
                ],
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{team}");
    assert_eq!(team["object"], "agent_team");
    assert_eq!(team["total"], json!(2));
    assert_eq!(team["done"], json!(0));
    let team_id = team["id"].as_str().expect("team id").to_string();

    // A dependent task cannot be claimed before its dependency is done.
    let (status, blocked) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t2/claim"),
            auth,
            Some(json!({"worker": "w1"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{blocked}");

    let (status, claimed) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t1/claim"),
            auth,
            Some(json!({"worker": "w1"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{claimed}");
    assert_eq!(claimed["tasks"][0]["state"], "claimed");
    assert_eq!(claimed["tasks"][0]["claimed_by"], "w1");

    // Claiming the same task twice loses with 403 and keeps the first owner.
    let (status, twice) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t1/claim"),
            auth,
            Some(json!({"worker": "w2"})),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{twice}");

    let (status, done) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t1/complete"),
            auth,
            Some(json!({"result": "ok"})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{done}");
    assert_eq!(done["done"], json!(1));
    assert_eq!(done["tasks"][0]["state"], "done");

    let (status, ready) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agent-teams/{team_id}/tasks/t2/claim"),
            auth,
            Some(json!({})),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{ready}");
    assert_eq!(ready["tasks"][1]["state"], "claimed");

    // Unknown tasks and teams are 404, not 500.
    for path in [
        format!("/v1/agent-teams/{team_id}/tasks/nope/claim"),
        "/v1/agent-teams/nope/tasks/t1/claim".to_string(),
    ] {
        let (status, body) = h.api.send(Method::POST, &path, auth, Some(json!({}))).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }

    let (status, teams) = h.api.send(Method::GET, "/v1/agent-teams", auth, None).await;
    assert_eq!(status, StatusCode::OK, "{teams}");
    assert!(teams["data"]
        .as_array()
        .expect("team list")
        .iter()
        .any(|t| t["id"] == json!(team_id)));
}

#[tokio::test]
async fn scheduled_jobs_crud_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();

    let (status, job) = h
        .api
        .send(
            Method::POST,
            "/v1/scheduled-jobs",
            auth,
            Some(json!({
                "name": format!("nightly-{tag}"),
                "every_secs": 3600,
                "payload": {"kind": "sweep"},
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{job}");
    assert_eq!(job["object"], "scheduled_job");
    assert_eq!(job["every_secs"], json!(3600));
    assert!(job["next_run_ms"].as_i64().unwrap_or_default() > 0);
    let job_id = job["id"].as_str().expect("job id").to_string();

    // Exactly one of the two schedules is required.
    for body in [
        json!({"name": "both", "every_secs": 60, "at_ms": 1_700_000_000_000i64}),
        json!({"name": "neither"}),
    ] {
        let (status, _) = h
            .api
            .send(Method::POST, "/v1/scheduled-jobs", auth, Some(body))
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    let (status, list) = h
        .api
        .send(Method::GET, "/v1/scheduled-jobs", auth, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list["data"]
        .as_array()
        .expect("job list")
        .iter()
        .any(|j| j["id"] == json!(job_id)));

    let (status, deleted) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/scheduled-jobs/{job_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    let (status, _) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/scheduled-jobs/{job_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn memory_relations_and_bounded_graph_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();
    let (_aid, sid) = agent_and_session(&h, auth, &tag).await;

    let mut ids = Vec::new();
    for (key, value) in [
        ("alpha", "the build finished"),
        ("beta", "the release shipped"),
    ] {
        let (status, item) = h
            .api
            .send(
                Method::POST,
                &format!("/v1/agents/sessions/{sid}/memory"),
                auth,
                Some(json!({"key": key, "value": value})),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{item}");
        assert_eq!(item["kind"], "long_term");
        ids.push(item["id"].as_str().expect("memory id").to_string());
    }

    let (status, edge) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/memory/relations"),
            auth,
            Some(json!({
                "from_id": ids[0],
                "to_id": ids[1],
                "kind": "causal",
                "score": 0.9,
            })),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{edge}");
    assert_eq!(edge["kind"], "causal");
    assert_eq!(edge["provenance"], "rest");

    // Filters narrow the edge list; an unknown kind is a client error.
    let (status, rels) = h
        .api
        .send(
            Method::GET,
            &format!(
                "/v1/agents/sessions/{sid}/memory/relations?from={}&kind=causal",
                ids[0]
            ),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{rels}");
    assert!(rels["data"]
        .as_array()
        .expect("relations")
        .iter()
        .any(|r| r["to_id"] == json!(ids[1]) && r["kind"] == "causal"));
    let (status, _) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/memory/relations?kind=nonsense"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // Bounded graph retrieval from a seed; without a seed it refuses.
    let (status, graph) = h
        .api
        .send(
            Method::GET,
            &format!(
                "/v1/agents/sessions/{sid}/memory/relations/graph?seeds={}&views=causal&max_hops=2&budget=10",
                ids[0]
            ),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    assert!(!graph["items"].as_array().expect("graph items").is_empty());
    assert_eq!(graph["trace"]["budget"], json!(10));
    assert!(graph["trace"]["stop_reason"].is_string());
    let (status, _) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{sid}/memory/relations/graph"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // A delete needs at least one selector, and then removes the edge for good.
    let (status, _) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/agents/sessions/{sid}/memory/relations"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, removed) = h
        .api
        .send(
            Method::DELETE,
            &format!("/v1/agents/sessions/{sid}/memory/relations?from={}", ids[0]),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{removed}");
    assert!(removed["deleted"].as_u64().unwrap_or_default() >= 1);
    let (status, after) = h
        .api
        .send(
            Method::GET,
            &format!(
                "/v1/agents/sessions/{sid}/memory/relations?from={}&kind=causal",
                ids[0]
            ),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["data"], json!([]));
}

#[tokio::test]
async fn subagent_reads_are_empty_and_scoped_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let tag = uuid::Uuid::new_v4();
    let (_aid, sid) = agent_and_session(&h, auth, &tag).await;

    // The projection does not invent subagents: an existing session answers
    // with empty, well-formed collections.
    for path in [
        format!("/v1/agents/sessions/{sid}/subagents"),
        format!("/v1/agents/sessions/{sid}/subagents/sa/items"),
        format!("/v1/agents/sessions/{sid}/subagents/sa/turns"),
    ] {
        let (status, body) = h.api.send(Method::GET, &path, auth, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["object"], json!("list"), "{path}");
        assert_eq!(body["data"], json!([]), "{path}");
    }

    // One subagent (and one turn) that never ran is a 404, not an empty object.
    for path in [
        format!("/v1/agents/sessions/{sid}/subagents/sa"),
        format!("/v1/agents/sessions/{sid}/subagents/sa/turns/t1"),
    ] {
        let (status, body) = h.api.send(Method::GET, &path, auth, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }

    // Every read is gated on the session: an unknown one is 404 even for the
    // list endpoints that would otherwise answer `[]`.
    for path in [
        "/v1/agents/sessions/nope/subagents",
        "/v1/agents/sessions/nope/subagents/sa",
        "/v1/agents/sessions/nope/subagents/sa/items",
        "/v1/agents/sessions/nope/subagents/sa/turns",
        "/v1/agents/sessions/nope/subagents/sa/turns/t1",
    ] {
        let (status, body) = h.api.send(Method::GET, path, auth, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{path}: {body}");
    }
}

#[tokio::test]
async fn unimplemented_resources_answer_501_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    // Recognised-but-unimplemented resources must be self-describing: an
    // OpenAI client enumerating the surface gets 501 + the resource name,
    // never a 404 that reads like a typo.
    for (method, path, resource) in [
        (Method::GET, "/v1/vaults", "vaults"),
        (Method::POST, "/v1/vaults", "vaults"),
        (Method::GET, "/v1/vaults/v1", "vaults"),
        (
            Method::GET,
            "/v1/vaults/v1/credentials",
            "vaults.credentials",
        ),
        (
            Method::GET,
            "/v1/agents/environments",
            "agents.environments",
        ),
        (
            Method::POST,
            "/v1/agents/environments",
            "agents.environments",
        ),
        (
            Method::GET,
            "/v1/agents/environments/e1",
            "agents.environments",
        ),
        (
            Method::GET,
            "/v1/agents/environments/e1/files",
            "agents.environments.files",
        ),
        (
            Method::POST,
            "/v1/agents/environments/e1/files",
            "agents.environments.files",
        ),
        (
            Method::GET,
            "/v1/agents/environments/templates",
            "agents.environments.templates",
        ),
        (
            Method::GET,
            "/v1/agents/sessions/s1/artifacts",
            "agents.sessions.artifacts",
        ),
    ] {
        let (status, body) = h.api.send(method.clone(), path, auth, None).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{method} {path}");
        assert_eq!(body["error"]["type"], json!("not_implemented"), "{path}");
        assert_eq!(body["error"]["resource"], json!(resource), "{path}");
    }
}

#[tokio::test]
async fn rsi_off_is_visible_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    let (status, body) = h.api.send(Method::GET, "/v1/rsi/status", auth, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "rsi_status");
    assert_eq!(body["enabled"], json!(false));
    assert!(body["gate"].is_null());
    assert_eq!(body["records"], json!(0));

    // The switch is off: a sweep refuses instead of pretending to run.
    let (status, body) = h
        .api
        .send(Method::POST, "/v1/rsi/sweep", auth, Some(json!({})))
        .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");

    // The read-only surfaces still answer with an empty list.
    for path in ["/v1/rsi/reports", "/v1/rsi/proposals"] {
        let (status, body) = h.api.send(Method::GET, path, auth, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {body}");
        assert_eq!(body["data"], json!([]), "{path}");
    }

    // A single missing object is a 404, never a fabricated empty shell; a
    // promotion/discard of an unknown proposal is also 404 (nothing to act
    // on), while the version list is an honest empty collection.
    let (status, body) = h
        .api
        .send(Method::GET, "/v1/rsi/reports/missing", auth, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = h
        .api
        .send(
            Method::GET,
            "/v1/rsi/playbooks/retry_storm:shell",
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = h
        .api
        .send(
            Method::GET,
            "/v1/rsi/playbooks/retry_storm:shell/versions",
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"], json!([]));
    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/v1/rsi/proposals/missing/discard",
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/v1/rsi/proposals/missing/promote",
            auth,
            Some(json!({"expected_revision": 0})),
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
}

#[tokio::test]
async fn unknown_routes_and_methods_answer_4xx_over_a_real_socket() {
    let h = memory_harness(stub_model(), stub_sandbox()).await;
    let (status, body) = h.api.send(Method::GET, "/v1/not-a-route", None, None).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    let (status, body) = h.api.send(Method::DELETE, "/healthz", None, None).await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED, "{body}");

    // An empty turn is a client error, not a stream that never starts.
    let token = bootstrap_token();
    let tag = uuid::Uuid::new_v4();
    let (_aid, sid) = agent_and_session(&h, token.as_deref(), &tag).await;
    let (status, body) = h
        .api
        .send(
            Method::POST,
            &format!("/v1/agents/sessions/{sid}/events/stream"),
            token.as_deref(),
            Some(json!({"text": "   "})),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

// ------------------------------------------------------------------- A2A
//
// The A2A surface is a second protocol on the same server: public discovery
// cards, a JSON-RPC endpoint behind the auth layer, and two SSE shapes (a
// streaming call and a replay). `a2a_tests.rs` pins the semantics in-process;
// these drive the same routes over a real socket, where chunked framing, the
// auth middleware and the console mirror all take part.

#[tokio::test]
async fn a2a_cards_scope_gates_and_tenancy_over_a_real_socket() {
    let h = a2a_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    // Discovery is public: an orchestrator must find the agent before it has a
    // key. The card also has to stay free of deployment internals.
    let (status, card_text) = h
        .api
        .call(Method::GET, "/.well-known/agent-card.json", None, None)
        .await;
    assert_eq!(status, StatusCode::OK, "{card_text}");
    let card: Value = serde_json::from_str(&card_text).expect("the card is JSON");
    assert_eq!(card["protocolVersion"], "1.0.0");
    assert_eq!(card["name"], A2A_AGENT_NAME);
    assert_eq!(card["url"], "https://cloud.example.test/a2a");
    assert_eq!(card["capabilities"]["streaming"], true);
    assert_eq!(card["capabilities"]["pushNotifications"], true);
    assert_eq!(card["authentication"]["schemes"][0], "bearer");
    assert_eq!(card["version"], env!("CARGO_PKG_VERSION"));
    for forbidden in ["instructions", "shell", "admin", "sess_"] {
        assert!(
            !card_text.contains(forbidden),
            "{forbidden} leaked into {card_text}"
        );
    }
    // The compatibility aliases answer, and the per-agent card points at that
    // agent's own endpoint.
    for path in ["/.well-known/agent.json", "/a2a/agent-card"] {
        let (status, _) = h.api.call(Method::GET, path, None, None).await;
        assert_eq!(status, StatusCode::OK, "{path}");
    }
    let (status, per_agent) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/{A2A_AGENT_ID}/agent-card.json"),
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{per_agent}");
    assert_eq!(
        per_agent["url"],
        format!("https://cloud.example.test/v1/agents/{A2A_AGENT_ID}/a2a")
    );
    let (status, missing) = h
        .api
        .send(
            Method::GET,
            "/v1/agents/agent-missing/agent-card.json",
            None,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(missing["error"]["code"], "agent_not_found");

    // The JSON-RPC route is behind the auth layer; in open mode the same call
    // answers as the default tenant instead of refusing.
    let read_call = rpc_call(1, "tasks/get", json!({"id": "task_missing"}));
    let (status, body) = h
        .api
        .send(Method::POST, "/a2a", None, Some(read_call))
        .await;
    assert_eq!(
        status,
        if auth.is_some() {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::OK
        },
        "{body}"
    );
    if auth.is_none() {
        assert_eq!(rpc_error(&body), -32001, "{body}");
    }

    // Scopes belong to the caller, not the method: a read-only key cannot start
    // a run, a write-only key cannot read one back or follow the replay.
    let tag = uuid::Uuid::new_v4();
    let (ro, wo) = (format!("a2a-ro-{tag}"), format!("a2a-wo-{tag}"));
    issue_key(&h, &ro, &a2a_owner().id, &["read"]).await;
    issue_key(&h, &wo, &a2a_owner().id, &["write"]).await;
    let send_call = rpc_call(2, "message/send", json!({"message": a2a_msg("scope gate")}));
    let (status, body) = h
        .api
        .send(Method::POST, "/a2a", Some(&ro), Some(send_call.clone()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_scope");
    let (status, started) = h
        .api
        .send(Method::POST, "/a2a", Some(&wo), Some(send_call))
        .await;
    assert_eq!(status, StatusCode::OK, "{started}");
    assert_eq!(started["result"]["status"]["state"], "completed");
    let task_id = started["result"]["id"]
        .as_str()
        .expect("task id")
        .to_string();
    let get_call = rpc_call(3, "tasks/get", json!({"id": task_id}));
    let (status, body) = h
        .api
        .send(Method::POST, "/a2a", Some(&wo), Some(get_call.clone()))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = h
        .api
        .send(Method::POST, "/a2a", Some(&ro), Some(get_call))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["id"], task_id);
    let (status, _) = h
        .api
        .send(
            Method::GET,
            &format!("/a2a/stream?task={task_id}"),
            Some(&wo),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, replay) = h
        .api
        .call(
            Method::GET,
            &format!("/a2a/stream?task={task_id}"),
            Some(&ro),
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(replay.contains("event: task_status_update"), "{replay}");

    // Another tenant sees neither the agent nor the task: the answer is the
    // protocol's machine code, not a hint about what exists.
    let outsider = format!("a2a-out-{tag}");
    issue_key(&h, &outsider, &format!("a2a-b-{tag}"), &["read", "write"]).await;
    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            Some(&outsider),
            Some(rpc_call(
                4,
                "message/send",
                json!({"message": a2a_msg("hello")}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(rpc_error(&body), -32004, "{body}");
    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            Some(&outsider),
            Some(rpc_call(5, "tasks/get", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc_error(&body), -32001, "{body}");

    // A deployment with A2A switched off still mounts the routes, so a probe
    // gets a reason instead of a router 404.
    let off = memory_harness(stub_model(), stub_sandbox()).await;
    let (status, body) = off
        .api
        .send(Method::GET, "/.well-known/agent-card.json", None, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "agent_not_found");
    let (status, body) = off
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                6,
                "message/send",
                json!({"message": a2a_msg("hello")}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(rpc_error(&body), -32004, "{body}");
}

#[tokio::test]
async fn a2a_task_lifecycle_and_console_mirror_over_a_real_socket() {
    let h = a2a_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();
    let prompt = "ping over a2a";

    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                1,
                "message/send",
                json!({"message": a2a_msg(prompt)}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["jsonrpc"], "2.0");
    assert_eq!(body["id"], 1);
    let task = &body["result"];
    assert_eq!(task["kind"], "task");
    let task_id = task["id"].as_str().expect("task id").to_string();
    assert!(task_id.starts_with("task_"), "{task_id}");
    let context = task["contextId"].as_str().expect("context id").to_string();
    assert!(context.starts_with("sess_"), "{context}");
    assert_eq!(task["status"]["state"], "completed");
    assert_eq!(task["history"][0]["role"], "user");
    let artifact = &task["artifacts"][0];
    assert_eq!(artifact["artifactId"], "final");
    let answer = artifact["parts"][0]["text"].as_str().expect("text part");
    assert!(answer.contains(prompt), "{answer}");
    // A part carries both discriminators; agenthub dispatches on `type`.
    assert_eq!(artifact["parts"][0]["kind"], "text");
    assert_eq!(artifact["parts"][0]["type"], "text");

    // The run is the console's own session: items and the finished turn are
    // readable through the OpenAI-shaped routes.
    let (status, items) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{context}/items"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{items}");
    let items = items["data"].as_array().expect("items list");
    assert!(items.iter().any(|i| i["role"] == "user"), "{items:?}");
    assert!(items.iter().any(|i| i["role"] == "assistant"), "{items:?}");
    let (status, turns) = h
        .api
        .send(
            Method::GET,
            &format!("/v1/agents/sessions/{context}/turns"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{turns}");
    assert_eq!(turns["data"][0]["status"], "completed");

    // `tasks/get` accepts both ids and trims the history on demand.
    let (status, full) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(2, "tasks/get", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{full}");
    assert_eq!(full["result"]["id"], task_id);
    assert_eq!(full["result"]["status"]["state"], "completed");
    assert!(!full["result"]["history"].as_array().unwrap().is_empty());
    let (_, aliased) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(3, "tasks/get", json!({"taskId": task_id}))),
        )
        .await;
    assert_eq!(aliased["result"]["id"], task_id);
    let (_, trimmed) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                4,
                "tasks/get",
                json!({"id": task_id, "historyLength": 0}),
            )),
        )
        .await;
    assert_eq!(trimmed["result"]["history"], json!([]), "{trimmed}");

    // Push configuration round-trips, and a URL that would smuggle credentials
    // is refused before it is stored or used.
    let (status, set) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                5,
                "tasks/pushNotification/set",
                json!({
                    "id": task_id,
                    "pushNotificationConfig": {"url": "https://hooks.example.test/a2a"},
                }),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{set}");
    assert_eq!(set["result"]["url"], "https://hooks.example.test/a2a");
    let (_, got) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                6,
                "tasks/pushNotification/get",
                json!({"id": task_id}),
            )),
        )
        .await;
    assert_eq!(got["result"]["url"], "https://hooks.example.test/a2a");

    // A second task with no hook: `get` answers `-32006`, and a bad URL is
    // `-32602` on both the push route and `message/send`.
    let (_, second) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                7,
                "message/send",
                json!({"message": a2a_msg("no hook")}),
            )),
        )
        .await;
    let second_id = second["result"]["id"]
        .as_str()
        .expect("second task id")
        .to_string();
    let (_, none) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                8,
                "tasks/pushNotification/get",
                json!({"id": second_id}),
            )),
        )
        .await;
    assert_eq!(rpc_error(&none), -32006, "{none}");
    let (_, bad_push) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                9,
                "tasks/pushNotification/set",
                json!({
                    "id": second_id,
                    "pushNotificationConfig": {"url": "https://user:pass@hooks.example.test/x"},
                }),
            )),
        )
        .await;
    assert_eq!(rpc_error(&bad_push), -32602, "{bad_push}");
    let (_, bad_send) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                10,
                "message/send",
                json!({
                    "message": a2a_msg("hi"),
                    "configuration": {"pushNotification": {"url": "ftp://hooks.example.test"}},
                }),
            )),
        )
        .await;
    assert_eq!(rpc_error(&bad_send), -32602, "{bad_send}");

    // A task that finished cannot be cancelled, an unknown one is `-32001`, and
    // an unknown method is `-32601` — all inside a 200 so the code is readable.
    let (_, cancelled) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(11, "tasks/cancel", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(rpc_error(&cancelled), -32002, "{cancelled}");
    let (status, unknown) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(12, "tasks/get", json!({"id": "task_missing"}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc_error(&unknown), -32001, "{unknown}");
    let (status, method) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(13, "tasks/teleport", json!({}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(rpc_error(&method), -32601, "{method}");

    // Envelope failures are transport-level: 400 with the JSON-RPC code, never
    // a 200 error body.
    let (status, parse) = h.api.post_raw("/a2a", auth, "{not json").await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(rpc_error(&parse), -32700, "{parse}");
    let (status, invalid) = h.api.post_raw("/a2a", auth, r#"{"jsonrpc":"2.0"}"#).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(rpc_error(&invalid), -32600, "{invalid}");
}

#[tokio::test]
async fn a2a_stream_and_replay_frames_over_a_real_socket() {
    let h = a2a_harness(stub_model(), stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    let (status, content_type, text) = h
        .api
        .call_with_type(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(
                7,
                "message/stream",
                json!({"message": a2a_msg("stream it")}),
            )),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert!(
        content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "{content_type:?}"
    );
    assert!(!text.contains("[DONE]"), "no sentinel allowed: {text}");
    let frames = sse_frames(&text);
    assert!(frames.len() >= 2, "{text}");
    // Every frame is a JSON-RPC result carrying the request id.
    for frame in &frames {
        assert_eq!(frame["jsonrpc"], "2.0");
        assert_eq!(frame["id"], 7);
    }
    assert_eq!(frames[0]["result"]["kind"], "task");
    let last = frames.last().unwrap();
    assert_eq!(last["result"]["kind"], "status-update");
    assert_eq!(last["result"]["final"], true);
    assert_eq!(last["result"]["status"]["state"], "completed");
    // The legacy dispatch names travel with the frames.
    assert!(text.contains("event: task_artifact_update"), "{text}");
    assert!(text.contains("event: task_status_update"), "{text}");
    let task_id = frames[0]["result"]["id"]
        .as_str()
        .expect("streamed task id")
        .to_string();

    // The replay endpoint serves the same task without a JSON-RPC envelope and
    // ends on its own once the terminal frame is out.
    let (status, content_type, replay) = h
        .api
        .call_with_type(
            Method::GET,
            &format!("/a2a/stream?task={task_id}"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{replay}");
    assert!(
        content_type
            .as_deref()
            .unwrap_or_default()
            .starts_with("text/event-stream"),
        "{content_type:?}"
    );
    let replayed = sse_frames(&replay);
    assert_eq!(replayed[0]["kind"], "task", "{replay}");
    assert!(
        replayed.iter().all(|f| f.get("jsonrpc").is_none()),
        "no envelope on the replay path: {replay}"
    );
    assert_eq!(
        replayed.last().unwrap()["status"]["state"],
        "completed",
        "{replay}"
    );
    assert_eq!(replayed.last().unwrap()["final"], true, "{replay}");

    // `since` past the end is an empty stream; a missing or unknown task is a
    // client error with a machine code.
    let (status, _, empty) = h
        .api
        .call_with_type(
            Method::GET,
            &format!("/a2a/stream?task={task_id}&since=9999"),
            auth,
            None,
        )
        .await;
    assert_eq!(status, StatusCode::OK);
    assert!(sse_frames(&empty).is_empty(), "{empty}");
    let (status, body) = h.api.send(Method::GET, "/a2a/stream", auth, None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error"]["code"], "invalid_params");
    let (status, body) = h
        .api
        .send(Method::GET, "/a2a/stream?task=task_missing", auth, None)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["code"], "task_not_found");
}

#[tokio::test]
async fn a2a_cancel_stops_a_live_stream_over_two_sockets() {
    // The model blocks until the test releases it, so the cancel cannot lose a
    // race with a fast stub: the run is still working when the second socket
    // asks for it to stop.
    let gate = Arc::new(tokio::sync::Notify::new());
    let make: MakeModel = {
        let gate = gate.clone();
        Arc::new(move || Box::new(GatedModel(gate.clone())) as Box<dyn ModelClient>)
    };
    let h = a2a_harness(make, stub_sandbox()).await;
    let token = bootstrap_token();
    let auth = token.as_deref();

    // `message/send` would block until the run ends; the stream returns as soon
    // as the task is announced, which is what makes a live cancel reachable.
    let mut req = h
        .api
        .http
        .post(format!("{}/a2a", h.api.base))
        .json(&rpc_call(
            1,
            "message/stream",
            json!({"message": a2a_msg("cancel me")}),
        ));
    if let Some(token) = auth {
        req = req.bearer_auth(token);
    }
    let mut resp = req.send().await.expect("the stream opens");
    assert_eq!(resp.status(), StatusCode::OK);
    let mut text = String::new();
    while !text.contains("event: task\n") {
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(10), resp.chunk())
            .await
            .expect("the stream announces the task")
            .expect("the stream stays open")
            .expect("a chunk arrives");
        text.push_str(&String::from_utf8_lossy(&chunk));
    }
    let task_id = sse_frames(&text)[0]["result"]["id"]
        .as_str()
        .expect("streamed task id")
        .to_string();

    // Cancel over a second connection while the first is still streaming.
    let (status, body) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(2, "tasks/cancel", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["status"]["state"], "canceled", "{body}");

    // The open stream ends on its own with the terminal frame.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while let Some(chunk) = resp.chunk().await.expect("the stream ends cleanly") {
            text.push_str(&String::from_utf8_lossy(&chunk));
        }
    })
    .await
    .expect("a cancelled stream is closed, not left hanging");

    let frames = sse_frames(&text);
    let last = frames.last().expect("the stream carried frames");
    assert_eq!(last["result"]["kind"], "status-update");
    assert_eq!(last["result"]["final"], true);
    assert_eq!(last["result"]["status"]["state"], "canceled");

    // The row agrees, and a second cancel loses to the terminal state.
    let (_, got) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(3, "tasks/get", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(got["result"]["status"]["state"], "canceled");
    let (_, again) = h
        .api
        .send(
            Method::POST,
            "/a2a",
            auth,
            Some(rpc_call(4, "tasks/cancel", json!({"id": task_id}))),
        )
        .await;
    assert_eq!(rpc_error(&again), -32002, "{again}");

    // The aborted run never consults the model again; release it anyway so no
    // waiter outlives the test.
    gate.notify_one();
}
