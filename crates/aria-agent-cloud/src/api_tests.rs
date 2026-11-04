//! End-to-end tests for the HTTP surface, driven through the real router with
//! in-memory stores. No database, no model provider, no listening socket.
//!
//! Environment-dependent behaviour (`AGENT_CLOUD_API_KEY`) is serialised with
//! [`ENV_LOCK`]: the variable is process-global while `cargo test` runs tests
//! on parallel threads.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use agent_core::context::{ContextStore, MemoryContextStore};
use agent_core::{CoreError, ModelClient, ModelRequest, ModelResponse, StubModel};
use agent_sandbox::{ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use serde_json::{json, Value};
use tower::ServiceExt;

use crate::app::{build_router, AppState, MakeContext, MakeModel, MakeSandbox, HEALTH_PATH};
use crate::auth::{ApiKeyScopes, KeyCache, Principal, PrincipalKind};
use crate::store::MemoryStore;

pub const ADMIN_KEY: &str = "test-admin-key";

static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Which auth mode the test wants; also the guard that keeps it in place.
pub struct EnvGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
}

pub async fn env_admin() -> EnvGuard {
    let g = ENV_LOCK.lock().await;
    std::env::set_var("AGENT_CLOUD_API_KEY", ADMIN_KEY);
    EnvGuard { _lock: g }
}

pub async fn env_open() -> EnvGuard {
    let g = ENV_LOCK.lock().await;
    std::env::remove_var("AGENT_CLOUD_API_KEY");
    EnvGuard { _lock: g }
}

/// Shared per-principal context stores (so a session's memory survives across
/// requests, exactly like the Postgres implementation).
#[derive(Clone, Default)]
pub struct Contexts(Arc<Mutex<HashMap<String, Arc<MemoryContextStore>>>>);

impl Contexts {
    pub fn store_for(&self, p: &Principal) -> Arc<dyn ContextStore> {
        let mut g = self.0.lock().unwrap();
        let entry = g.entry(p.id.clone()).or_default();
        entry.clone() as Arc<dyn ContextStore>
    }
}

/// In-process sandbox: records the command it was asked to run.
pub struct StubSandbox;

#[async_trait]
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

/// A model that asks for one tool call, then answers once the tool result is in
/// the context (for the SSE tool-call persistence path).
pub struct ToolCallModel;

#[async_trait]
impl ModelClient for ToolCallModel {
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
        Ok(ModelResponse {
            text: format!("final: {}", req.input),
        })
    }

    async fn complete_with_tools(
        &self,
        req: &ModelRequest,
        _tools: &[agent_core::Tool],
    ) -> Result<agent_core::ModelTurn, CoreError> {
        if req.context.contains("TOOL_RESULT") {
            Ok(agent_core::ModelTurn {
                text: "done after tool".into(),
                tool_calls: Vec::new(),
            })
        } else {
            Ok(agent_core::ModelTurn {
                text: String::new(),
                tool_calls: vec![agent_core::ToolCall {
                    id: "call_1".into(),
                    name: "shell".into(),
                    arguments: json!({"command": ["echo", "hi"]}),
                }],
            })
        }
    }
}

/// A model that always fails, for the streaming error path.
pub struct FailingModel;

#[async_trait]
impl ModelClient for FailingModel {
    async fn complete(&self, _req: &ModelRequest) -> Result<ModelResponse, CoreError> {
        Err(CoreError::Model("model exploded".into()))
    }
}

pub struct Harness {
    pub app: Router,
    pub state: AppState,
}

pub fn harness_with(
    make_model: impl Fn() -> Box<dyn ModelClient> + Send + Sync + 'static,
) -> Harness {
    let mem = Arc::new(MemoryStore::new());
    let contexts = Contexts::default();
    let ctx = contexts.clone();
    let make_context: MakeContext = Arc::new(move |p: &Principal| ctx.store_for(p));
    let make_model: MakeModel = Arc::new(make_model);
    let make_sandbox: MakeSandbox = Arc::new(|| Arc::new(StubSandbox) as Arc<dyn Sandbox>);
    let state = AppState {
        agents: mem.clone(),
        sessions: mem.clone(),
        keys: mem,
        key_cache: KeyCache::new(Duration::from_secs(60)),
        make_model,
        make_sandbox,
        make_context,
    };
    Harness {
        app: build_router(state.clone()),
        state,
    }
}

pub fn harness() -> Harness {
    harness_with(|| Box::new(StubModel::new("agent")))
}

/// Same harness, but the context store can be made to fail on chosen calls.
pub fn harness_with_context(fail_recall: bool, fail_relations: bool) -> Harness {
    let mem = Arc::new(MemoryStore::new());
    let contexts = Contexts::default();
    let inner = contexts.clone();
    let make_context: MakeContext = Arc::new(move |p: &Principal| {
        Arc::new(FaultyContext {
            inner: inner.store_for(p),
            fail_recall,
            fail_relations,
        }) as Arc<dyn ContextStore>
    });
    let make_model: MakeModel = Arc::new(|| Box::new(StubModel::new("agent")));
    let make_sandbox: MakeSandbox = Arc::new(|| Arc::new(StubSandbox) as Arc<dyn Sandbox>);
    let state = AppState {
        agents: mem.clone(),
        sessions: mem.clone(),
        keys: mem,
        key_cache: KeyCache::new(Duration::from_secs(60)),
        make_model,
        make_sandbox,
        make_context,
    };
    Harness {
        app: build_router(state.clone()),
        state,
    }
}

/// Context store wrapper that can fail on `recall` (write→connect) and/or
/// `get_relations` (stream summary), everything else delegates.
pub struct FaultyContext {
    pub inner: Arc<dyn ContextStore>,
    pub fail_recall: bool,
    pub fail_relations: bool,
}

#[async_trait]
impl ContextStore for FaultyContext {
    async fn memorize(
        &self,
        frag: agent_core::context::ContextFragment,
    ) -> Result<(), agent_core::context::ContextError> {
        self.inner.memorize(frag).await
    }

    async fn recall(
        &self,
        query: &agent_core::context::RecallQuery,
    ) -> Result<Vec<agent_core::context::ContextFragment>, agent_core::context::ContextError> {
        if self.fail_recall {
            return Err(agent_core::context::ContextError::Storage(
                "injected recall failure".into(),
            ));
        }
        self.inner.recall(query).await
    }

    async fn compact(
        &self,
        session: &str,
    ) -> Result<agent_core::context::ContextFragment, agent_core::context::ContextError> {
        self.inner.compact(session).await
    }

    async fn get_by_key(
        &self,
        session: &str,
        key: &str,
    ) -> Result<Option<agent_core::context::ContextFragment>, agent_core::context::ContextError>
    {
        self.inner.get_by_key(session, key).await
    }

    async fn list_session(
        &self,
        session: &str,
        top_k: usize,
    ) -> Result<Vec<agent_core::context::ContextFragment>, agent_core::context::ContextError> {
        self.inner.list_session(session, top_k).await
    }

    async fn relate(
        &self,
        rel: agent_core::context::Relation,
    ) -> Result<(), agent_core::context::ContextError> {
        self.inner.relate(rel).await
    }

    async fn get_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<agent_core::context::RelationKind>,
        top_k: usize,
    ) -> Result<Vec<agent_core::context::Relation>, agent_core::context::ContextError> {
        if self.fail_relations {
            return Err(agent_core::context::ContextError::Storage(
                "injected relations failure".into(),
            ));
        }
        self.inner
            .get_relations(session, from, to, kind, top_k)
            .await
    }

    async fn delete_relations(
        &self,
        session: &str,
        from: Option<&str>,
        to: Option<&str>,
        kind: Option<agent_core::context::RelationKind>,
    ) -> Result<usize, agent_core::context::ContextError> {
        self.inner.delete_relations(session, from, to, kind).await
    }

    async fn expand(
        &self,
        query: &agent_core::context::GraphRetrieveQuery,
    ) -> Result<agent_core::context::GraphRetrieveResult, agent_core::context::ContextError> {
        self.inner.expand(query).await
    }
}

/// Send one request; returns the status and the decoded JSON body (`null` for
/// an empty body).
pub async fn send(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let (status, bytes) = send_bytes(app, method, uri, token, body).await;
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

pub async fn send_bytes(
    app: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Vec<u8>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(t) = token {
        builder = builder.header("authorization", format!("Bearer {t}"));
    }
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .expect("build request"),
        None => builder.body(Body::empty()).expect("build request"),
    };
    let resp = app
        .clone()
        .oneshot(request)
        .await
        .expect("router should respond");
    let status = resp.status();
    let bytes = http_body_util::BodyExt::collect(resp.into_body())
        .await
        .expect("read body")
        .to_bytes()
        .to_vec();
    (status, bytes)
}

/// Create an agent as the admin and return its id.
pub async fn create_agent(h: &Harness, name: &str) -> String {
    let (status, body) = send(
        &h.app,
        "POST",
        "/v1/agents",
        Some(ADMIN_KEY),
        Some(json!({"name": name})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create agent: {body}");
    body["id"].as_str().expect("agent id").to_string()
}

/// Create a session for `agent_id` as the admin and return its id.
pub async fn create_session(h: &Harness, agent_id: &str) -> String {
    let (status, body) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions",
        Some(ADMIN_KEY),
        Some(json!({"agent_id": agent_id})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "create session: {body}");
    body["id"].as_str().expect("session id").to_string()
}

// ---------------------------------------------------------------- health/auth

#[tokio::test]
async fn health_is_reachable_without_a_key_while_routes_are_gated() {
    let _env = env_admin().await;
    let h = harness();
    let (gated, _) = send(&h.app, "GET", "/v1/agents", None, None).await;
    let (health, bytes) = send_bytes(&h.app, "GET", HEALTH_PATH, None, None).await;
    assert_eq!(gated, StatusCode::UNAUTHORIZED);
    assert_eq!(health, StatusCode::OK);
    assert_eq!(bytes, b"ok\n");
}

#[tokio::test]
async fn open_mode_serves_requests_as_the_default_tenant() {
    let _env = env_open().await;
    let h = harness();
    let (status, body) = send(&h.app, "GET", "/v1/agents", None, None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["object"], "list");
}

#[tokio::test]
async fn bootstrap_key_resolves_to_admin_and_wrong_keys_are_rejected() {
    let _env = env_admin().await;
    let h = harness();
    let (ok, body) = send(&h.app, "GET", "/v1/agents", Some(ADMIN_KEY), None).await;
    assert_eq!(ok, StatusCode::OK, "{body}");
    let (bad, _) = send(&h.app, "GET", "/v1/agents", Some("nope"), None).await;
    assert_eq!(bad, StatusCode::UNAUTHORIZED);
    let (empty, _) = send(&h.app, "GET", "/v1/agents", Some(""), None).await;
    assert_eq!(empty, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn api_key_from_the_store_authenticates_and_is_cached() {
    let _env = env_admin().await;
    let h = harness();
    let (status, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "tenant-a", "scopes": ["read"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let key = created["key"].as_str().unwrap().to_string();
    assert_eq!(created["principal_id"], "tenant-a");
    assert_eq!(created["scopes"], json!(["read"]));

    // First request resolves through the store and caches the principal.
    let (ok, body) = send(&h.app, "GET", "/v1/agents", Some(&key), None).await;
    assert_eq!(ok, StatusCode::OK, "{body}");
    // Second request hits the cache (same result, no store round-trip).
    let (again, _) = send(&h.app, "GET", "/v1/agents", Some(&key), None).await;
    assert_eq!(again, StatusCode::OK);
}

#[tokio::test]
async fn tenant_sees_only_its_own_agents() {
    let _env = env_admin().await;
    let h = harness();
    let (_, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "tenant-a", "scopes": ["read", "write"]})),
    )
    .await;
    let key = created["key"].as_str().unwrap().to_string();

    // Admin owns one agent; the tenant creates another.
    create_agent(&h, "admin-agent").await;
    let (status, tenant_agent) = send(
        &h.app,
        "POST",
        "/v1/agents",
        Some(&key),
        Some(json!({"name": "tenant-agent"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{tenant_agent}");

    let (_, tenant_list) = send(&h.app, "GET", "/v1/agents", Some(&key), None).await;
    assert_eq!(tenant_list["data"].as_array().unwrap().len(), 1);
    assert_eq!(tenant_list["data"][0]["name"], "tenant-agent");

    let (_, admin_list) = send(&h.app, "GET", "/v1/agents", Some(ADMIN_KEY), None).await;
    assert_eq!(admin_list["data"].as_array().unwrap().len(), 2);

    // And the tenant cannot read the admin's agent by id.
    let (_, everything) = send(&h.app, "GET", "/v1/agents", Some(ADMIN_KEY), None).await;
    let admin_owned = everything["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["name"] == "admin-agent")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    let (cross, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/{admin_owned}"),
        Some(&key),
        None,
    )
    .await;
    assert_eq!(cross, StatusCode::NOT_FOUND);
}

// ------------------------------------------------------------------ api keys

#[tokio::test]
async fn api_keys_require_admin_scope() {
    let _env = env_admin().await;
    let h = harness();
    let (_, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "tenant-b"})),
    )
    .await;
    let key = created["key"].as_str().unwrap().to_string();

    let (create, _) = send(&h.app, "POST", "/v1/api-keys", Some(&key), Some(json!({}))).await;
    assert_eq!(create, StatusCode::FORBIDDEN);
    let (list, _) = send(&h.app, "GET", "/v1/api-keys", Some(&key), None).await;
    assert_eq!(list, StatusCode::FORBIDDEN);
    let (revoke, _) = send(&h.app, "DELETE", "/v1/api-keys/x", Some(&key), None).await;
    assert_eq!(revoke, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn api_key_lifecycle_list_revoke_and_404() {
    let _env = env_admin().await;
    let h = harness();
    let (status, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"label": "ci", "scopes": ["admin"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    let key = created["key"].as_str().unwrap().to_string();
    assert!(key.starts_with("aria-"));
    assert_eq!(created["scopes"], json!(["admin", "read", "write"]));
    // A generated principal id is a UUID (safe as a shard key).
    assert_eq!(created["principal_id"].as_str().unwrap().len(), 36);

    let (status, list) = send(&h.app, "GET", "/v1/api-keys", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["label"], "ci");

    // Resolve it once so the cache holds an entry, then revoke.
    let (before, _) = send(&h.app, "GET", "/v1/agents", Some(&key), None).await;
    assert_eq!(before, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/api-keys/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (after, _) = send(&h.app, "GET", "/v1/agents", Some(&key), None).await;
    assert_eq!(
        after,
        StatusCode::UNAUTHORIZED,
        "revocation must be immediate"
    );
    let (again, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/api-keys/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(again, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn api_key_creation_validates_the_principal_id() {
    let _env = env_admin().await;
    let h = harness();
    for bad in ["", "has space", &"x".repeat(65), "semi;colon"] {
        let (status, body) = send(
            &h.app,
            "POST",
            "/v1/api-keys",
            Some(ADMIN_KEY),
            Some(json!({"principal_id": bad})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}: {body}");
    }
}

// -------------------------------------------------------------------- agents

#[tokio::test]
async fn agents_crud_roundtrip() {
    let _env = env_admin().await;
    let h = harness();

    let (status, created) = send(
        &h.app,
        "POST",
        "/v1/agents",
        Some(ADMIN_KEY),
        Some(json!({
            "name": "demo",
            "model": "gpt-4o-mini",
            "instructions": "be brief",
            "reasoning": {"effort": "low", "summary": "auto"},
            "multi_agent": {"enabled": true, "max_concurrent_subagents": 3},
            "service_tier": "flex",
            "text": {"format": "text", "verbosity": "low"},
            "tools": [{"type": "function", "name": "shell"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["object"], "agent");
    assert_eq!(created["reasoning"]["effort"], "low");
    assert_eq!(created["multi_agent"]["max_concurrent_subagents"], 3);
    assert_eq!(created["service_tier"], "flex");
    assert_eq!(created["text"]["format"], "text");
    assert_eq!(created["tools"][0]["name"], "shell");

    // Read by id and by human-friendly name.
    let (by_id, fetched) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(by_id, StatusCode::OK);
    assert_eq!(fetched["id"], id.as_str());
    let (by_name, named) = send(&h.app, "GET", "/v1/agents/demo", Some(ADMIN_KEY), None).await;
    assert_eq!(by_name, StatusCode::OK);
    assert_eq!(named["id"], id.as_str());

    // Partial update touches only the provided fields.
    let (status, updated) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{id}"),
        Some(ADMIN_KEY),
        Some(json!({"instructions": "be very brief", "reasoning": {"effort": "high"}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["instructions"], "be very brief");
    assert_eq!(updated["reasoning"]["effort"], "high");
    assert_eq!(updated["name"], "demo", "untouched fields survive");
    assert_eq!(updated["service_tier"], "flex");

    let (status, deleted) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["object"], "agent.deleted");
    assert_eq!(deleted["deleted"], true);

    let (gone, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(gone, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn agent_defaults_and_list_limit_clamping() {
    let _env = env_admin().await;
    let h = harness();
    let (status, created) = send(
        &h.app,
        "POST",
        "/v1/agents",
        Some(ADMIN_KEY),
        Some(json!({})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created["service_tier"], "auto");
    assert_eq!(created["tools"], json!([]));
    assert!(created.get("text").is_none());

    // limit is clamped into [1, 100].
    let (_, one) = send(&h.app, "GET", "/v1/agents?limit=0", Some(ADMIN_KEY), None).await;
    assert_eq!(one["data"].as_array().unwrap().len(), 1);
    let (_, many) = send(
        &h.app,
        "GET",
        "/v1/agents?limit=1000",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(many["data"].as_array().unwrap().len(), 1);
    // Unparseable limit is rejected by the query extractor (400).
    let (bad, _) = send(
        &h.app,
        "GET",
        "/v1/agents?limit=nope",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn agent_not_found_and_store_failures() {
    let _env = env_admin().await;
    let h = harness();
    let missing = "/v1/agents/nope";
    for (method, body) in [
        ("GET", None),
        ("POST", Some(json!({"name": "x"}))),
        ("DELETE", None),
    ] {
        let (status, _) = send(&h.app, method, missing, Some(ADMIN_KEY), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {missing}");
    }
}

// ------------------------------------------------------------------ sessions

#[tokio::test]
async fn session_crud_roundtrip_and_validation() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "session-agent").await;

    // `agent_id` / `agent` are both accepted; neither is a 400.
    let sess = create_session(&h, &agent_id).await;
    let (status, by_name) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions",
        Some(ADMIN_KEY),
        Some(json!({"agent": "session-agent", "instructions": "extra"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{by_name}");
    assert_eq!(by_name["agent"]["name"], "session-agent");
    assert_eq!(by_name["instructions"], "extra");

    let (missing, _) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions",
        Some(ADMIN_KEY),
        Some(json!({})),
    )
    .await;
    assert_eq!(missing, StatusCode::BAD_REQUEST);
    let (unknown, _) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions",
        Some(ADMIN_KEY),
        Some(json!({"agent_id": "nope"})),
    )
    .await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);

    let (status, fetched) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(fetched["object"], "agent.session");
    assert_eq!(fetched["agent"]["id"], agent_id.as_str());

    // Update instructions, then omit them (unchanged).
    let (status, updated) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        Some(json!({"instructions": "session override"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{updated}");
    assert_eq!(updated["instructions"], "session override");
    let (_, unchanged) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        Some(json!({})),
    )
    .await;
    assert_eq!(unchanged["instructions"], "session override");

    // Listing clamps the limit and resolves each agent.
    let (_, listed) = send(
        &h.app,
        "GET",
        "/v1/agents/sessions?limit=0",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(listed["object"], "list");
    assert_eq!(listed["data"].as_array().unwrap().len(), 1);

    let (status, deleted) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted["object"], "agent.session.deleted");
    let (gone, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(gone, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn sessions_of_deleted_agents_are_not_listed() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "doomed").await;
    let _sess = create_session(&h, &agent_id).await;
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/{agent_id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, listed) = send(&h.app, "GET", "/v1/agents/sessions", Some(ADMIN_KEY), None).await;
    assert_eq!(listed["data"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn blocking_turn_persists_items_turns_and_rejects_empty_input() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "turn-agent").await;
    let sess = create_session(&h, &agent_id).await;

    let (empty, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events"),
        Some(ADMIN_KEY),
        Some(json!({"input": "   "})),
    )
    .await;
    assert_eq!(empty, StatusCode::BAD_REQUEST);

    let (status, turn) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events"),
        Some(ADMIN_KEY),
        Some(json!({"input": "hello"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{turn}");
    assert_eq!(turn["object"], "agent.session.turn");
    assert_eq!(turn["status"], "completed");
    assert!(turn["output"].as_str().unwrap().contains("hello"));

    let (_, items) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let items = items["data"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "user");
    assert_eq!(items[1]["role"], "assistant");

    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let turn_id = turns["data"][0]["id"].as_str().unwrap().to_string();
    let (status, one) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns/{turn_id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(one["id"], turn_id.as_str());

    let (missing, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns/nope"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);

    // A second turn exercises ordering with more than one row.
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events"),
        Some(ADMIN_KEY),
        Some(json!({"input": "again"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(turns["data"].as_array().unwrap().len(), 2);

    // Deleting a session drops its turns and items with it.
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for uri in [
        format!("/v1/agents/sessions/{sess}/items"),
        format!("/v1/agents/sessions/{sess}/turns"),
    ] {
        let (gone, _) = send(&h.app, "GET", &uri, Some(ADMIN_KEY), None).await;
        assert_eq!(gone, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn turn_errors_are_mapped_to_http_statuses() {
    let _env = env_admin().await;
    let h = harness_with(|| Box::new(FailingModel));
    let agent_id = create_agent(&h, "failing").await;
    let sess = create_session(&h, &agent_id).await;

    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events"),
        Some(ADMIN_KEY),
        Some(json!({"input": "boom"})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("model exploded"));

    let (unknown, _) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions/nope/events",
        Some(ADMIN_KEY),
        Some(json!({"input": "hi"})),
    )
    .await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn streaming_turn_emits_terminal_frame_and_persists_items() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "stream-agent").await;
    let sess = create_session(&h, &agent_id).await;

    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "stream me"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    let frames = parse_sse(&text);
    assert!(!frames.is_empty(), "no frames: {text}");
    let types: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert_eq!(types.first().copied(), Some("agent.turn.created"));
    assert_eq!(types.last().copied(), Some("agent.turn.completed"));
    assert!(!text.contains("[DONE]"), "no sentinel allowed: {text}");
    // Sequence numbers are monotonic and unique.
    let seqs: Vec<u64> = frames
        .iter()
        .map(|f| f["sequence_number"].as_u64().unwrap())
        .collect();
    let mut sorted = seqs.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted, seqs);
    // Every frame carries the session/turn id.
    for f in &frames {
        assert_eq!(f["session_id"], sess.as_str());
        assert!(f["turn_id"].is_string());
    }

    let (_, items) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let items = items["data"].as_array().unwrap();
    assert!(items.iter().any(|i| i["role"] == "assistant"));

    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(turns["data"][0]["status"], "completed");
}

#[tokio::test]
async fn streaming_turn_error_paths() {
    let _env = env_admin().await;
    let h = harness_with(|| Box::new(FailingModel));
    let agent_id = create_agent(&h, "stream-fail").await;
    let sess = create_session(&h, &agent_id).await;

    let (empty, _) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": ""})),
    )
    .await;
    assert_eq!(empty, StatusCode::BAD_REQUEST);

    let (unknown, _) = send_bytes(
        &h.app,
        "POST",
        "/v1/agents/sessions/nope/events/stream",
        Some(ADMIN_KEY),
        Some(json!({"input": "hi"})),
    )
    .await;
    assert_eq!(unknown, StatusCode::NOT_FOUND);

    // The model fails inside the stream → a terminal `agent.turn.failed`
    // frame and a persisted `failed` turn.
    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "boom"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    let frames = parse_sse(&text);
    assert_eq!(
        frames.last().unwrap()["type"],
        "agent.turn.failed",
        "{text}"
    );
    let (_, turns) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(turns["data"][0]["status"], "failed");
}

#[tokio::test]
async fn streaming_turn_records_tool_calls() {
    let _env = env_admin().await;
    let h = harness_with(|| Box::new(ToolCallModel));
    let agent_id = create_agent(&h, "tool-agent").await;
    let sess = create_session(&h, &agent_id).await;

    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "run a tool"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let text = String::from_utf8(bytes).unwrap();
    let frames = parse_sse(&text);
    let types: Vec<&str> = frames.iter().map(|f| f["type"].as_str().unwrap()).collect();
    assert_eq!(
        types.last().copied(),
        Some("agent.turn.completed"),
        "{text}"
    );
    assert!(
        types.contains(&"agent.turn.item.done"),
        "tool call items must be emitted: {text}"
    );

    let (_, items) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    let items = items["data"].as_array().unwrap();
    assert!(
        items.iter().any(|i| i["type"] == "function_call"),
        "tool call must be persisted: {items:?}"
    );
}

#[tokio::test]
async fn streaming_turn_attaches_the_memory_relation_summary() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "rel-stream").await;
    let sess = create_session(&h, &agent_id).await;
    let (a, b) = seed_two_memories(&h, &sess).await;
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/memory/relations"),
        Some(ADMIN_KEY),
        Some(json!({"from_id": a, "to_id": b, "kind": "semantic"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "with relations"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frames = parse_sse(&String::from_utf8(bytes).unwrap());
    let last = frames.last().unwrap();
    assert_eq!(last["type"], "agent.turn.completed");
    let summary = last["memory_relations"]
        .as_array()
        .unwrap_or_else(|| panic!("relation summary missing: {last}"));
    assert!(!summary.is_empty(), "{last}");
}

#[tokio::test]
async fn graph_retrieval_defaults_to_every_view() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "graph-agent").await;
    let sess = create_session(&h, &agent_id).await;
    let (a, _b) = seed_two_memories(&h, &sess).await;

    let (status, graph) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/memory/relations/graph?seeds={a}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    assert_eq!(graph["trace"]["budget"], 60);
    assert!(graph["trace"]["stop_reason"].is_string());

    // Explicit budget/max_hops/top_k are honoured.
    let (status, tuned) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/memory/relations/graph?seeds={a}&budget=5&max_hops=1&top_k=2"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(tuned["trace"]["budget"], 5);
}

#[tokio::test]
async fn session_instructions_matrix() {
    let _env = env_admin().await;
    let h = harness();
    // Agent with its own instructions.
    let (_, agent) = send(
        &h.app,
        "POST",
        "/v1/agents",
        Some(ADMIN_KEY),
        Some(json!({"name": "with-instructions", "instructions": "base"})),
    )
    .await;
    let agent_id = agent["id"].as_str().unwrap().to_string();

    for (instructions, label) in [
        (Some("session"), "append"),
        (Some("   "), "blank session instructions are ignored"),
        (None, "no session instructions"),
    ] {
        let (status, sess) = send(
            &h.app,
            "POST",
            "/v1/agents/sessions",
            Some(ADMIN_KEY),
            Some(json!({"agent_id": agent_id, "instructions": instructions})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: {sess}");
        let sess_id = sess["id"].as_str().unwrap().to_string();
        let (status, turn) = send(
            &h.app,
            "POST",
            &format!("/v1/agents/sessions/{sess_id}/events"),
            Some(ADMIN_KEY),
            Some(json!({"input": "hi"})),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{label}: {turn}");
    }

    // Agent with no instructions + session instructions (the "extra wins" case).
    let plain = create_agent(&h, "plain").await;
    let (_, sess) = send(
        &h.app,
        "POST",
        "/v1/agents/sessions",
        Some(ADMIN_KEY),
        Some(json!({"agent_id": plain, "instructions": "only session"})),
    )
    .await;
    let sess_id = sess["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess_id}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn every_sandbox_provider_builds_and_unknown_is_rejected() {
    let _env = env_admin().await;
    let h = harness();
    let principal = Principal::admin();
    for provider in ["codex", "browser", "docker", "kata", "cube"] {
        let mut cfg = crate::app::agent_config("a", "", "s");
        cfg.sandbox_provider = provider.into();
        if let Err(e) = crate::app::build_agent_with_config(&h.state, cfg, &principal).await {
            panic!("{provider} should build: {e:?}");
        }
    }
    let mut bad = crate::app::agent_config("a", "", "s");
    bad.sandbox_provider = "nope".into();
    assert!(
        crate::app::build_agent_with_config(&h.state, bad, &principal)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn principal_extractor_requires_the_middleware() {
    use axum::extract::FromRequestParts;
    let request = Request::builder().uri("/").body(()).unwrap();
    let (mut parts, _) = request.into_parts();
    let rejected = <Principal as FromRequestParts<()>>::from_request_parts(&mut parts, &()).await;
    assert!(rejected.is_err());

    // With the extension present it succeeds.
    let mut request = Request::builder().uri("/").body(()).unwrap();
    request.extensions_mut().insert(Principal::admin());
    let (mut parts, _) = request.into_parts();
    let ok = <Principal as FromRequestParts<()>>::from_request_parts(&mut parts, &()).await;
    assert_eq!(ok.unwrap(), Principal::admin());
}

/// Parse an SSE body into its JSON frames.
pub fn parse_sse(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .filter(|d| *d != "[DONE]")
        .filter_map(|d| serde_json::from_str::<Value>(d).ok())
        .collect()
}

// -------------------------------------------------------------------- memory

#[tokio::test]
async fn memory_write_read_and_recall() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "memory-agent").await;
    let sess = create_session(&h, &agent_id).await;
    let base = format!("/v1/agents/sessions/{sess}/memory");

    let (status, written) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"key": "fact-1", "value": "the sky is blue", "kind": "note"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{written}");
    assert_eq!(written["key"], "fact-1");
    assert_eq!(written["kind"], "note");

    // Unkeyed + default kind.
    let (status, plain) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"value": "an unkeyed fragment", "kind": "message"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(plain.get("key").is_none() || plain["key"].is_null());
    assert_eq!(plain["kind"], "message");

    // A blank key is not a key.
    let (_, blank) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"key": "   ", "value": "blank key"})),
    )
    .await;
    assert_eq!(blank["kind"], "long_term");

    let (status, read) = send(
        &h.app,
        "GET",
        &format!("{base}/fact-1"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(read["content"], "the sky is blue");

    let (missing, _) = send(
        &h.app,
        "GET",
        &format!("{base}/nope"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(missing, StatusCode::NOT_FOUND);

    // Recall with text, then list without text.
    let (status, recalled) = send(
        &h.app,
        "GET",
        &format!("{base}?text=sky&top_k=5"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(recalled["data"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["content"] == "the sky is blue"));
    // No query text lists the whole session (and `top_k` is clamped).
    let (status, clamped) = send(
        &h.app,
        "GET",
        &format!("{base}?top_k=0"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        clamped["data"].as_array().unwrap().len(),
        1,
        "top_k clamps to 1"
    );

    let (status, all) = send(
        &h.app,
        "GET",
        &format!("{base}?top_k=10"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["data"].as_array().unwrap().len(), 3, "{all}");
    // A blank query behaves like no query.
    let (status, blank) = send(
        &h.app,
        "GET",
        &format!("{base}?text=%20&top_k=10"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(blank["data"].as_array().unwrap().len(), 3, "{blank}");
}

#[tokio::test]
async fn memory_rejects_unknown_kinds_and_missing_sessions() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "memory-agent").await;
    let sess = create_session(&h, &agent_id).await;

    let (bad_kind, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/memory"),
        Some(ADMIN_KEY),
        Some(json!({"value": "x", "kind": "nope"})),
    )
    .await;
    assert_eq!(bad_kind, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("unknown fragment kind"));

    for (method, uri, body) in [
        (
            "POST",
            "/v1/agents/sessions/nope/memory".to_string(),
            Some(json!({"value": "x"})),
        ),
        ("GET", "/v1/agents/sessions/nope/memory".to_string(), None),
        ("GET", "/v1/agents/sessions/nope/memory/k".to_string(), None),
    ] {
        let (status, _) = send(&h.app, method, &uri, Some(ADMIN_KEY), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

// ------------------------------------------------------------------ relations

async fn seed_two_memories(h: &Harness, sess: &str) -> (String, String) {
    let base = format!("/v1/agents/sessions/{sess}/memory");
    let (_, a) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"value": "rust ownership prevents data races"})),
    )
    .await;
    let (_, b) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"value": "rust ownership avoids memory bugs"})),
    )
    .await;
    (
        a["id"].as_str().unwrap().to_string(),
        b["id"].as_str().unwrap().to_string(),
    )
}

#[tokio::test]
async fn relations_roundtrip_list_filter_delete_and_graph() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "rel-agent").await;
    let sess = create_session(&h, &agent_id).await;
    let (a, b) = seed_two_memories(&h, &sess).await;
    let base = format!("/v1/agents/sessions/{sess}/memory/relations");

    // Defaults: score 0.8, provenance "rest".
    let (status, created) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"from_id": a, "to_id": b, "kind": "entity"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    assert_eq!(created["score"].as_f64().unwrap(), 0.8);
    assert_eq!(created["provenance"], "rest");
    assert_eq!(created["from_id"], a.as_str());

    // Explicit score/provenance + kind filter.
    let (status, created2) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(
            json!({"from_id": a, "to_id": b, "kind": "causal", "score": 0.5, "provenance": "test"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(created2["provenance"], "test");

    let (status, listed) = send(
        &h.app,
        "GET",
        &format!("{base}?kind=entity"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{listed}");
    assert!(listed["data"]
        .as_array()
        .unwrap()
        .iter()
        .all(|r| r["kind"] == "entity"));
    assert!(!listed["data"].as_array().unwrap().is_empty());

    // Filter by from/to and clamp top_k.
    let (_, by_from) = send(
        &h.app,
        "GET",
        &format!("{base}?from={a}&to={b}&kind=&top_k=0"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert!(!by_from["data"].as_array().unwrap().is_empty());

    let (bad_kind, _) = send(
        &h.app,
        "GET",
        &format!("{base}?kind=nope"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(bad_kind, StatusCode::BAD_REQUEST);

    // Graph retrieval needs a seed, views are optional, bad views are rejected.
    let (status, graph) = send(
        &h.app,
        "GET",
        &format!("{base}/graph?seeds={a},&views=entity,causal"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{graph}");
    assert!(!graph["items"].as_array().unwrap().is_empty());
    assert!(!graph["trace"]["views"].as_array().unwrap().is_empty());
    assert_eq!(graph["trace"]["budget"], 60);

    let (no_seed, _) = send(
        &h.app,
        "GET",
        &format!("{base}/graph?seeds=%20"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(no_seed, StatusCode::BAD_REQUEST);
    let (bad_view, _) = send(
        &h.app,
        "GET",
        &format!("{base}/graph?seeds={a}&views=nope"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(bad_view, StatusCode::BAD_REQUEST);

    // Deleting without a filter is refused.
    let (no_filter, _) = send(&h.app, "DELETE", &base, Some(ADMIN_KEY), None).await;
    assert_eq!(no_filter, StatusCode::BAD_REQUEST);
    let (bad_delete_kind, _) = send(
        &h.app,
        "DELETE",
        &format!("{base}?kind=nope"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(bad_delete_kind, StatusCode::BAD_REQUEST);

    let (status, deleted) = send(
        &h.app,
        "DELETE",
        &format!("{base}?from={a}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{deleted}");
    assert!(deleted["deleted"].as_u64().unwrap() >= 1);
    let (status, deleted_by_kind) = send(
        &h.app,
        "DELETE",
        &format!("{base}?kind=entity"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(deleted_by_kind["object"], "relation.deleted");
    // And by `to`.
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("{base}?to={b}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn relations_reject_bad_kinds_and_missing_sessions() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "rel-agent").await;
    let sess = create_session(&h, &agent_id).await;
    let base = format!("/v1/agents/sessions/{sess}/memory/relations");

    let (bad, _) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"from_id": "a", "to_id": "b", "kind": "bogus"})),
    )
    .await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);

    // Unknown endpoints → the store's `relate` error becomes a 400.
    let (dangling, body) = send(
        &h.app,
        "POST",
        &base,
        Some(ADMIN_KEY),
        Some(json!({"from_id": "missing-a", "to_id": "missing-b", "kind": "semantic"})),
    )
    .await;
    assert_eq!(dangling, StatusCode::BAD_REQUEST, "{body}");

    for (method, uri, body) in [
        (
            "POST",
            "/v1/agents/sessions/nope/memory/relations".to_string(),
            Some(json!({"from_id":"a","to_id":"b","kind":"semantic"})),
        ),
        (
            "GET",
            "/v1/agents/sessions/nope/memory/relations".to_string(),
            None,
        ),
        (
            "DELETE",
            "/v1/agents/sessions/nope/memory/relations?from=a".to_string(),
            None,
        ),
        (
            "GET",
            "/v1/agents/sessions/nope/memory/relations/graph?seeds=a".to_string(),
            None,
        ),
    ] {
        let (status, _) = send(&h.app, method, &uri, Some(ADMIN_KEY), body).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
    }
}

// ----------------------------------------------------------------- subagents

#[tokio::test]
async fn subagent_endpoints_are_empty_but_well_formed() {
    let _env = env_admin().await;
    let h = harness();
    let agent_id = create_agent(&h, "sub-agent").await;
    let sess = create_session(&h, &agent_id).await;

    for uri in [
        format!("/v1/agents/sessions/{sess}/subagents"),
        format!("/v1/agents/sessions/{sess}/subagents/sa/items"),
        format!("/v1/agents/sessions/{sess}/subagents/sa/turns"),
    ] {
        let (status, body) = send(&h.app, "GET", &uri, Some(ADMIN_KEY), None).await;
        assert_eq!(status, StatusCode::OK, "{uri}: {body}");
        assert_eq!(body["object"], "list");
        assert_eq!(body["data"].as_array().unwrap().len(), 0);
    }
    for uri in [
        format!("/v1/agents/sessions/{sess}/subagents/sa"),
        format!("/v1/agents/sessions/{sess}/subagents/sa/turns/t1"),
    ] {
        let (status, _) = send(&h.app, "GET", &uri, Some(ADMIN_KEY), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }

    // Every read is gated on the session being visible.
    for uri in [
        "/v1/agents/sessions/nope/subagents".to_string(),
        "/v1/agents/sessions/nope/subagents/sa".to_string(),
        "/v1/agents/sessions/nope/subagents/sa/items".to_string(),
        "/v1/agents/sessions/nope/subagents/sa/turns".to_string(),
        "/v1/agents/sessions/nope/subagents/sa/turns/t1".to_string(),
        "/v1/agents/sessions/nope/items".to_string(),
        "/v1/agents/sessions/nope/turns".to_string(),
        "/v1/agents/sessions/nope/turns/t1".to_string(),
    ] {
        let (status, _) = send(&h.app, "GET", &uri, Some(ADMIN_KEY), None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{uri}");
    }
}

// -------------------------------------------------------------------- stubs

#[tokio::test]
async fn unimplemented_resources_return_501() {
    let _env = env_open().await;
    let h = harness();
    for (method, uri) in [
        ("GET", "/v1/vaults"),
        ("POST", "/v1/vaults"),
        ("GET", "/v1/vaults/v1"),
        ("GET", "/v1/vaults/v1/credentials"),
        ("GET", "/v1/agents/environments"),
        ("POST", "/v1/agents/environments"),
        ("GET", "/v1/agents/environments/e1"),
        ("GET", "/v1/agents/environments/e1/files"),
        ("POST", "/v1/agents/environments/e1/files"),
        ("GET", "/v1/agents/environments/templates"),
        ("GET", "/v1/agents/sessions/s1/artifacts"),
    ] {
        let (status, body) = send(&h.app, method, uri, None, None).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED, "{method} {uri}");
        assert_eq!(body["error"]["type"], "not_implemented");
    }
}

// --------------------------------------------------------- context failures

#[tokio::test]
async fn context_failures_are_swallowed_by_memory_writes_and_streams() {
    let _env = env_admin().await;

    // (1) A failing write→connect recall must not fail the memory write, and
    //     must surface as a 400 (not a panic) on the recall endpoint.
    let h = harness_with_context(true, false);
    let agent_id = create_agent(&h, "ctx-fail").await;
    let sess = create_session(&h, &agent_id).await;
    let (status, body) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/memory"),
        Some(ADMIN_KEY),
        Some(json!({"value": "hello", "key": "k"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/memory?text=hello"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    // (2) A failing relation summary must not break the stream; the terminal
    //     frame simply carries no `memory_relations`.
    let h = harness_with_context(false, true);
    let agent_id = create_agent(&h, "ctx-rel-fail").await;
    let sess = create_session(&h, &agent_id).await;
    let (status, bytes) = send_bytes(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events/stream"),
        Some(ADMIN_KEY),
        Some(json!({"input": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let frames = parse_sse(&String::from_utf8(bytes).unwrap());
    let last = frames.last().unwrap();
    assert_eq!(last["type"], "agent.turn.completed");
    assert_eq!(last["memory_relations"], json!([]));
}

// ------------------------------------------------------------ store failures

/// Wrap the memory store so every call fails: proves handlers map storage
/// errors to 500 instead of panicking.
mod faulty {
    use super::*;
    use crate::store::memory::faulty::FaultyStore;
    use crate::store::MemoryStore;

    pub fn harness() -> Harness {
        let mem = Arc::new(MemoryStore::new());
        let contexts = Contexts::default();
        let ctx = contexts.clone();
        let make_context: MakeContext = Arc::new(move |p: &Principal| ctx.store_for(p));
        let make_model: MakeModel = Arc::new(|| Box::new(StubModel::new("agent")));
        let make_sandbox: MakeSandbox = Arc::new(|| Arc::new(StubSandbox) as Arc<dyn Sandbox>);
        let broken = Arc::new(FaultyStore {
            inner: mem.clone(),
            sessions: mem.clone(),
            keys: mem.clone(),
        });
        let state = AppState {
            agents: broken.clone(),
            sessions: broken.clone(),
            keys: broken,
            key_cache: KeyCache::new(Duration::from_secs(60)),
            make_model,
            make_sandbox,
            make_context,
        };
        Harness {
            app: build_router(state.clone()),
            state,
        }
    }
}

#[tokio::test]
async fn storage_failures_map_to_500() {
    let _env = env_admin().await;
    let h = faulty::harness();
    let cases: Vec<(&str, String, Option<Value>)> = vec![
        ("POST", "/v1/agents".into(), Some(json!({"name": "x"}))),
        ("GET", "/v1/agents".into(), None),
        ("GET", "/v1/agents/a1".into(), None),
        ("POST", "/v1/agents/a1".into(), Some(json!({"name": "x"}))),
        ("DELETE", "/v1/agents/a1".into(), None),
        (
            "POST",
            "/v1/agents/sessions".into(),
            Some(json!({"agent_id": "a1"})),
        ),
        ("GET", "/v1/agents/sessions".into(), None),
        ("GET", "/v1/agents/sessions/s1".into(), None),
        (
            "POST",
            "/v1/agents/sessions/s1".into(),
            Some(json!({"instructions": "x"})),
        ),
        ("DELETE", "/v1/agents/sessions/s1".into(), None),
        (
            "POST",
            "/v1/agents/sessions/s1/events".into(),
            Some(json!({"input": "hi"})),
        ),
        ("GET", "/v1/agents/sessions/s1/items".into(), None),
        ("GET", "/v1/agents/sessions/s1/turns".into(), None),
        ("GET", "/v1/agents/sessions/s1/turns/t1".into(), None),
        ("GET", "/v1/agents/sessions/s1/subagents".into(), None),
        ("GET", "/v1/agents/sessions/s1/memory".into(), None),
        ("POST", "/v1/api-keys".into(), Some(json!({}))),
        ("GET", "/v1/api-keys".into(), None),
        ("DELETE", "/v1/api-keys/k1".into(), None),
        (
            "POST",
            "/v1/agents/sessions/s1/events/stream".into(),
            Some(json!({"input": "hi"})),
        ),
    ];
    for (method, uri, body) in cases {
        let (status, payload) = send(&h.app, method, &uri, Some(ADMIN_KEY), body).await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{method} {uri}: {payload}"
        );
    }
}

// ------------------------------------------- deep session-write failures

const FAIL_SET_INSTRUCTIONS: &[&str] = &["set_instructions"];
const FAIL_INSERT_TURN: &[&str] = &["insert_turn"];
const FAIL_INSERT_ITEM: &[&str] = &["insert_item"];
const FAIL_FINISH_TURN: &[&str] = &["finish_turn"];
const FAIL_GET_TURN: &[&str] = &["get_turn"];

pub fn harness_with_session_failure(fail: &'static [&'static str]) -> Harness {
    use crate::store::memory::faulty::FlakySessionStore;
    let mem = Arc::new(MemoryStore::new());
    let contexts = Contexts::default();
    let ctx = contexts.clone();
    let flaky = Arc::new(FlakySessionStore {
        inner: mem.clone(),
        agents: mem.clone(),
        keys: mem.clone(),
        fail,
    });
    let make_context: MakeContext = Arc::new(move |p: &Principal| ctx.store_for(p));
    let state = AppState {
        agents: flaky.clone(),
        sessions: flaky.clone(),
        keys: flaky,
        key_cache: KeyCache::new(Duration::from_secs(60)),
        make_model: Arc::new(|| Box::new(StubModel::new("agent"))),
        make_sandbox: Arc::new(|| Arc::new(StubSandbox) as Arc<dyn Sandbox>),
        make_context,
    };
    Harness {
        app: build_router(state.clone()),
        state,
    }
}

#[tokio::test]
async fn flaky_store_without_failures_delegates_every_operation() {
    let _env = env_admin().await;
    let h = harness_with_session_failure(&[]);
    let agent = create_agent(&h, "delegate").await;
    // Agent update/delete go through the delegating `AgentStore` impl.
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/{agent}"),
        Some(ADMIN_KEY),
        Some(json!({"instructions": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Keys go through the delegating `ApiKeyStore` impl.
    let (status, created) = send(
        &h.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "tenant-z"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{created}");
    let (status, listed) = send(&h.app, "GET", "/v1/api-keys", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let key_id = created["id"].as_str().unwrap().to_string();
    // A request authenticated with that key resolves it through the store.
    let tenant_key = created["key"].as_str().unwrap().to_string();
    let (status, agents) = send(&h.app, "GET", "/v1/agents", Some(&tenant_key), None).await;
    assert_eq!(status, StatusCode::OK, "{agents}");
    // The tenant owns no agent of its own (the admin's is invisible).
    assert_eq!(agents["data"].as_array().unwrap().len(), 0);
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/api-keys/{key_id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // The admin listing (delegated `list_agents`) still shows both tenants.
    let (status, all) = send(&h.app, "GET", "/v1/agents", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(all["data"].as_array().unwrap().len(), 1);

    // Session create/list/get/delete, turn/item writes and reads.
    let sess = create_session(&h, &agent).await;
    let (status, _) = send(&h.app, "GET", "/v1/agents/sessions", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}/events"),
        Some(ADMIN_KEY),
        Some(json!({"input": "hi"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/items"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = send(
        &h.app,
        "DELETE",
        &format!("/v1/agents/{agent}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn session_write_failures_map_to_500() {
    let _env = env_admin().await;

    // 1. Instruction update.
    let h = harness_with_session_failure(FAIL_SET_INSTRUCTIONS);
    let agent = create_agent(&h, "sf1").await;
    let sess = create_session(&h, &agent).await;
    let (status, _) = send(
        &h.app,
        "POST",
        &format!("/v1/agents/sessions/{sess}"),
        Some(ADMIN_KEY),
        Some(json!({"instructions": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);

    // 2. Turn/item writes inside the blocking turn driver.
    for fail in [FAIL_INSERT_TURN, FAIL_INSERT_ITEM, FAIL_FINISH_TURN] {
        let h = harness_with_session_failure(fail);
        let agent = create_agent(&h, "sf2").await;
        let sess = create_session(&h, &agent).await;
        let (status, body) = send(
            &h.app,
            "POST",
            &format!("/v1/agents/sessions/{sess}/events"),
            Some(ADMIN_KEY),
            Some(json!({"input": "hi"})),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::INTERNAL_SERVER_ERROR,
            "{fail:?}: {body}"
        );
    }

    // 3. Reading a single turn.
    let h = harness_with_session_failure(FAIL_GET_TURN);
    let agent = create_agent(&h, "sf3").await;
    let sess = create_session(&h, &agent).await;
    let (status, _) = send(
        &h.app,
        "GET",
        &format!("/v1/agents/sessions/{sess}/turns/anything"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[test]
fn sandbox_errors_map_to_core_config_errors() {
    let err = crate::AppError::from(agent_sandbox::SandboxError::Exec("boom".into()));
    assert!(format!("{err:?}").contains("boom"));
}

// ------------------------------------------------ pure store / auth helpers

#[test]
fn scopes_and_key_generation() {
    assert_eq!(
        ApiKeyScopes::from_tokens(&["read".into()]).to_tokens(),
        vec!["read".to_string()]
    );
    assert_eq!(
        ApiKeyScopes::from_tokens(&["write".into(), "bogus".into()]).to_tokens(),
        vec!["write".to_string()]
    );
    assert_eq!(ApiKeyScopes::default().to_tokens(), Vec::<String>::new());
    assert_eq!(
        ApiKeyScopes::all().to_tokens(),
        vec!["admin", "read", "write"]
    );
    assert!(!ApiKeyScopes::read_write().admin);

    let (key, prefix, hash) = crate::auth::generate_key();
    assert!(key.starts_with("aria-"));
    assert_eq!(key.len(), 5 + 36);
    assert!(key.starts_with(&prefix));
    assert_eq!(prefix.len(), 5 + 12);
    assert_eq!(hash, crate::auth::hash_key(&key));
    assert_ne!(crate::auth::generate_key().0, key);
}

#[test]
fn principal_helpers() {
    assert!(Principal::admin().is_admin());
    assert!(!Principal::default_user().is_admin());
    assert!(Principal {
        id: "x".into(),
        kind: PrincipalKind::User,
        scopes: ApiKeyScopes::all(),
    }
    .is_admin());
}

#[test]
fn agent_request_helpers_map_defaults_and_partial_updates() {
    use crate::store::{agent_from_request, apply_agent_update};
    use crate::types::{AgentReasoning, CreateAgentRequest, MultiAgentConfig, UpdateAgentRequest};

    let agent = agent_from_request("agent_1".into(), CreateAgentRequest::default(), 7).unwrap();
    assert_eq!(agent.id, "agent_1");
    assert_eq!(agent.object, "agent");
    assert_eq!(agent.service_tier, "auto");
    assert_eq!(agent.created_at, 7);
    assert!(agent.tools.is_empty());

    let mut current = agent;
    apply_agent_update(
        &mut current,
        UpdateAgentRequest {
            name: Some("named".into()),
            model: Some("m".into()),
            instructions: Some("i".into()),
            reasoning: Some(AgentReasoning {
                effort: Some("high".into()),
                summary: None,
            }),
            multi_agent: Some(MultiAgentConfig {
                enabled: true,
                max_concurrent_subagents: Some(2),
            }),
            service_tier: Some("flex".into()),
            text: None,
            tools: Some(vec![json!({"type": "function"})]),
        },
    )
    .unwrap();
    assert_eq!(current.name.as_deref(), Some("named"));
    assert_eq!(current.reasoning.effort.as_deref(), Some("high"));
    assert!(current.multi_agent.enabled);
    assert_eq!(current.service_tier, "flex");
    assert_eq!(current.tools.len(), 1);

    // `text` is only applied when provided.
    apply_agent_update(
        &mut current,
        UpdateAgentRequest {
            text: Some(crate::types::AgentText {
                format: Some("json".into()),
                verbosity: Some("high".into()),
            }),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(
        current.text.as_ref().and_then(|t| t.format.as_deref()),
        Some("json")
    );

    // An empty update changes nothing.
    let before = serde_json::to_value(&current).unwrap();
    apply_agent_update(&mut current, UpdateAgentRequest::default()).unwrap();
    assert_eq!(serde_json::to_value(&current).unwrap(), before);
}

#[test]
fn key_cache_ttl_and_purge() {
    let cache = KeyCache::new(Duration::from_secs(60));
    assert!(cache.get("h").is_none());
    cache.put("h", "t", ApiKeyScopes::read_write(), PrincipalKind::User);
    assert_eq!(cache.get("h").unwrap().principal_id, "t");
    assert!(cache.purge("h"));
    assert!(!cache.purge("h"));

    let expired = KeyCache::new(Duration::from_secs(0));
    expired.put("h", "t", ApiKeyScopes::all(), PrincipalKind::Admin);
    assert!(expired.get("h").is_none());
}

#[test]
fn memory_store_scopes_reads_and_sorting() {
    // Synchronous assertions over the in-memory store, exercised directly so
    // ordering/limit/tenant rules are pinned independently of the handlers.
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        use crate::store::{AgentStore, MemoryStore};
        let store = MemoryStore::new();
        let admin = Principal::admin();
        let tenant = Principal {
            id: "tenant".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        };
        for (id, ts) in [("a1", 1), ("a2", 3), ("a3", 2)] {
            let agent = crate::types::Agent {
                id: id.into(),
                object: "agent".into(),
                name: Some(format!("agent {id}")),
                model: None,
                instructions: None,
                reasoning: Default::default(),
                multi_agent: Default::default(),
                service_tier: "auto".into(),
                text: None,
                tools: vec![],
                created_at: ts,
            };
            store.insert_agent(&tenant, &agent).await.unwrap();
        }
        let listed = store.list_agents(&tenant, 2).await.unwrap();
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].id, "a2", "newest first");
        assert_eq!(listed[1].id, "a3");
        // Admin sees the same rows, and `limit` is honoured.
        assert_eq!(store.list_agents(&admin, 10).await.unwrap().len(), 3);
        assert_eq!(store.list_agents(&admin, 0).await.unwrap().len(), 0);
        // A different tenant sees nothing.
        let other = Principal {
            id: "other".into(),
            kind: PrincipalKind::User,
            scopes: ApiKeyScopes::read_write(),
        };
        assert!(store.list_agents(&other, 10).await.unwrap().is_empty());
        // Name lookup and unknown ids.
        assert_eq!(store.get_agent(&tenant, "agent a1").await.unwrap().id, "a1");
        assert!(store.get_agent(&tenant, "nope").await.is_err());
        // Saving an unknown agent is a NotFound.
        let mut ghost = store.get_agent(&tenant, "a1").await.unwrap();
        ghost.id = "ghost".into();
        assert!(store.save_agent(&ghost).await.is_err());
    });
}

#[test]
fn store_scope_string_helpers() {
    use crate::auth::ApiKeyScopes;
    use crate::store::{scopes_from_string, scopes_to_string};

    assert_eq!(scopes_to_string(ApiKeyScopes::all()), "admin,read,write");
    assert_eq!(scopes_from_string("read,,write,"), vec!["read", "write"]);
    assert!(scopes_from_string("").is_empty());
}
