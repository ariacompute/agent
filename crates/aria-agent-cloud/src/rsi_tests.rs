//! HTTP-level tests for the recursive self-improvement surface.
//!
//! Driven through the real router with in-memory providers: no database, no
//! model, no listening socket. What they pin down is the contract a caller
//! depends on — 503 when the feature is off, 409 on a stale revision, 422 on an
//! unadapted cross-tenant copy, 403 for a tenant reaching into another's work.

use std::sync::Arc;

use agent_core::seams::{Adaptation, PlaybookCandidate, PlaybookStore, RsiConfig, RsiScope};
use agent_core::session::{SessionEvent, SessionPersistence, SessionRecord};
use aria_agent_rsi::{DeterministicReflection, MemoryPlaybooks, RsiLedger, RsiSweep};
use axum::http::StatusCode;
use serde_json::json;

use crate::api_tests::{env_admin, harness, harness_with_rsi, send, Harness, ADMIN_KEY};
use crate::app::RsiState;
use crate::auth::Principal;
use crate::store::{MemoryStore, RsiStore, StoreReportSink};

const KEY: &str = "repeated_tool_failure:shell";

/// A session that has provably repeated a tool failure.
fn failure_log() -> Vec<SessionRecord> {
    let mut log = vec![SessionRecord {
        seq: 0,
        at_ms: 0,
        event: SessionEvent::TurnStarted {
            turn_id: "t1".into(),
            input: "go".into(),
        },
    }];
    for (index, item) in ["a", "b"].iter().enumerate() {
        let base = 1 + (index as u64) * 2;
        log.push(SessionRecord {
            seq: base,
            at_ms: base as i64,
            event: SessionEvent::ItemAdded {
                item_id: (*item).into(),
                kind: "tool_call".into(),
                payload: json!({"name": "shell", "arguments": {"cmd": ["ls"]}}),
            },
        });
        log.push(SessionRecord {
            seq: base + 1,
            at_ms: (base + 1) as i64,
            event: SessionEvent::ItemDone {
                item_id: (*item).into(),
                result: json!({"ok": false}),
                is_error: true,
                latency_ms: Some(15),
            },
        });
    }
    log.push(SessionRecord {
        seq: 5,
        at_ms: 5,
        event: SessionEvent::TurnCompleted {
            turn_id: "t1".into(),
            output: "ok".into(),
        },
    });
    log
}

struct Fixture {
    harness: Harness,
    playbooks: Arc<MemoryPlaybooks>,
}

fn enabled(auto_promote: bool) -> Fixture {
    let config = RsiConfig {
        enabled: true,
        auto_promote,
        sweep_threshold: 1,
        ..RsiConfig::default()
    };
    let ledger = Arc::new(RsiLedger::new(&config));
    ledger.append("admin", "s1", &failure_log()).unwrap();
    let playbooks = Arc::new(MemoryPlaybooks::new());
    let store: Arc<dyn RsiStore> = Arc::new(MemoryStore::new());
    let sweep = RsiSweep::new(
        ledger.clone(),
        Arc::new(DeterministicReflection::new(config.clone())),
        playbooks.clone(),
        config.clone(),
    )
    .with_reports(Arc::new(StoreReportSink::new(store.clone())));
    let rsi = RsiState::new(
        store,
        playbooks.clone(),
        Some(Arc::new(sweep)),
        Some(ledger),
        config,
    );
    Fixture {
        harness: harness_with_rsi(rsi),
        playbooks,
    }
}

fn candidate(id: &str) -> PlaybookCandidate {
    PlaybookCandidate {
        proposal_id: id.into(),
        origin: RsiScope::new("admin", "default"),
        scope: RsiScope::new("admin", "default"),
        key: KEY.into(),
        title: "Check before retrying".into(),
        body: "1. read the result first".into(),
        score: 90,
        baseline: 70,
        evidence: Vec::new(),
        adaptation: None,
    }
}

fn adaptation(to_domain: &str) -> serde_json::Value {
    json!({
        "from_principal": "admin",
        "from_domain": "default",
        "to_principal": "admin",
        "to_domain": to_domain,
        "rewritten": ["approval_threshold"],
        "rationale": "the target approves above a different threshold"
    })
}

#[tokio::test]
async fn rsi_is_off_by_default_and_says_so() {
    let _env = env_admin().await;
    let h = harness();
    let (status, body) = send(&h.app, "GET", "/v1/rsi/status", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["enabled"], false);
    assert!(body["gate"].is_null());

    let (status, _) = send(&h.app, "POST", "/v1/rsi/sweep", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);

    let (status, body) = send(&h.app, "GET", "/v1/rsi/reports", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 0);

    let (status, body) = send(&h.app, "GET", "/v1/rsi/proposals", Some(ADMIN_KEY), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 0);

    let (status, _) = send(
        &h.app,
        "GET",
        "/v1/rsi/playbooks/foo",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_auto_gate_publishes_and_every_artifact_is_queryable() {
    let _env = env_admin().await;
    let fx = enabled(true);
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/sweep",
        Some(ADMIN_KEY),
        Some(json!({"domain": "default"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body["promoted"].as_u64().unwrap() >= 1, "{body}");
    assert_eq!(body["discarded"], 0);

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        "/v1/rsi/status",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["enabled"], true);
    assert_eq!(body["gate"], "auto");
    assert!(body["records"].as_u64().unwrap() >= 6);

    // Everything was published, so nothing is pending.
    let (status, body) = send(
        &fx.harness.app,
        "GET",
        "/v1/rsi/proposals",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 0);

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        &format!("/v1/rsi/playbooks/{KEY}?domain=default"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revision"], 1);
    assert_eq!(body["object"], "rsi_playbook");
    assert!(
        body["scope"].get("principal").is_none(),
        "the tenant id stays internal"
    );

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        &format!("/v1/rsi/playbooks/{KEY}/versions?domain=default"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 1);

    // The diagnosis itself is stored, with its evidence chain.
    let (status, body) = send(
        &fx.harness.app,
        "GET",
        "/v1/rsi/reports",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let reports = body["data"].as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["session"], "s1");
    assert!(!reports[0]["findings"].as_array().unwrap().is_empty());
    let id = reports[0]["id"].as_str().unwrap().to_string();

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        &format!("/v1/rsi/reports/{id}"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["id"], id);

    let (status, _) = send(
        &fx.harness.app,
        "GET",
        "/v1/rsi/reports/missing",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_human_gate_needs_a_promote_and_guards_the_revision() {
    let _env = env_admin().await;
    let fx = enabled(false);
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/sweep",
        Some(ADMIN_KEY),
        Some(json!({"domain": "default"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["promoted"], 0);
    assert!(body["submitted"].as_u64().unwrap() >= 1, "{body}");

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        "/v1/rsi/proposals",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let pending = body["data"].as_array().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0]["key"], KEY);
    assert_eq!(pending[0]["adapted"], false);
    let id = pending[0]["id"].as_str().unwrap().to_string();

    // Nothing is in force before the approval.
    let (status, _) = send(
        &fx.harness.app,
        "GET",
        &format!("/v1/rsi/playbooks/{KEY}?domain=default"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(
        &fx.harness.app,
        "POST",
        &format!("/v1/rsi/proposals/{id}/promote"),
        Some(ADMIN_KEY),
        Some(json!({"expected_revision": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revision"], 1);

    // Promoting the same proposal twice is a missing row, not a second version.
    let (status, _) = send(
        &fx.harness.app,
        "POST",
        &format!("/v1/rsi/proposals/{id}/promote"),
        Some(ADMIN_KEY),
        Some(json!({"expected_revision": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A stale expectation conflicts instead of overwriting revision 1.
    fx.playbooks.submit(candidate("p2")).await.unwrap();
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/proposals/p2/promote",
        Some(ADMIN_KEY),
        Some(json!({"expected_revision": 0})),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        fx.playbooks.pending_count(),
        1,
        "the candidate survives a conflict"
    );

    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/proposals/p2/promote",
        Some(ADMIN_KEY),
        Some(json!({"expected_revision": 1})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["revision"], 2);

    let (status, body) = send(
        &fx.harness.app,
        "GET",
        &format!("/v1/rsi/playbooks/{KEY}/versions?domain=default"),
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["data"].as_array().unwrap().len(), 2);

    let (status, _) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/proposals/missing/discard",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // Discarding a real candidate answers with what was dropped.
    fx.playbooks.submit(candidate("p3")).await.unwrap();
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/proposals/p3/discard",
        Some(ADMIN_KEY),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["id"], "p3");
}

#[tokio::test]
async fn cross_tenant_adaptation_must_be_complete_and_privileged() {
    let _env = env_admin().await;
    let fx = enabled(false);
    fx.playbooks.submit(candidate("p1")).await.unwrap();
    fx.playbooks.promote("p1", 0).await.unwrap();

    let (status, body) = send(
        &fx.harness.app,
        "POST",
        &format!("/v1/rsi/playbooks/{KEY}/adapt"),
        Some(ADMIN_KEY),
        Some(json!({"domain": "billing", "adaptation": adaptation("billing")})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["adapted"], true);
    assert_eq!(body["scope"]["domain"], "billing");
    assert_eq!(
        fx.playbooks.pending_count(),
        1,
        "adapting only queues a proposal"
    );

    // An adaptation that names a different destination is refused, not applied.
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        &format!("/v1/rsi/playbooks/{KEY}/adapt"),
        Some(ADMIN_KEY),
        Some(json!({"domain": "billing", "adaptation": adaptation("payroll")})),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");

    // A user may not read another tenant's playbook to adapt it.
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/api-keys",
        Some(ADMIN_KEY),
        Some(json!({"principal_id": "tenant-u"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let user_key = body["key"].as_str().unwrap().to_string();
    let (status, _) = send(
        &fx.harness.app,
        "POST",
        &format!("/v1/rsi/playbooks/{KEY}/adapt"),
        Some(&user_key),
        Some(json!({"domain": "billing", "adaptation": adaptation("billing")})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // An unknown source is a 404, never an empty adaptation.
    let (status, _) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/playbooks/nope:tool/adapt",
        Some(ADMIN_KEY),
        Some(json!({"domain": "billing", "adaptation": adaptation("billing")})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_sweep_for_an_unknown_session_is_a_no_op() {
    let _env = env_admin().await;
    let fx = enabled(false);
    let (status, body) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/sweep",
        Some(ADMIN_KEY),
        Some(json!({"session": "never-seen", "domain": "default"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["sessions"], 1);
    assert_eq!(body["reports"], 0);

    // A blank session id is a client error, not a silent no-op.
    let (status, _) = send(
        &fx.harness.app,
        "POST",
        "/v1/rsi/sweep",
        Some(ADMIN_KEY),
        Some(json!({"session": "   ", "domain": "default"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_session_log_chain_feeds_the_tenant_ledger() {
    let fx = enabled(false);
    let log = crate::app::rsi_session_log(&fx.harness.state, &Principal::admin());
    assert_eq!(
        log.len(),
        2,
        "titles are authoritative, the ledger is second"
    );
    log.append("s9", &failure_log()).await.unwrap();
    let stats = fx.harness.state.rsi.ledger.as_ref().unwrap().stats();
    assert_eq!(stats.sessions, 2, "the seeded session plus s9");
    assert_eq!(
        fx.harness
            .state
            .rsi
            .ledger
            .as_ref()
            .unwrap()
            .records("admin", "s9")
            .len(),
        6
    );
}

#[tokio::test]
async fn a_state_without_a_ledger_keeps_a_single_sink() {
    let h = harness();
    let log = crate::app::rsi_session_log(&h.state, &Principal::admin());
    assert_eq!(log.len(), 1, "RSI off: nothing extra is written per turn");
}

#[test]
fn the_wire_types_never_leak_the_tenant() {
    let playbook = agent_core::seams::Playbook {
        key: KEY.into(),
        scope: RsiScope::new("tenant-secret", "orders"),
        origin: RsiScope::new("tenant-secret", "orders"),
        revision: 4,
        title: "t".into(),
        body: "b".into(),
        score: 90,
        source_proposal: "p".into(),
        adaptation: None,
        updated_at_ms: 1,
    };
    let view = serde_json::to_value(crate::api::rsi::PlaybookView::from(playbook)).unwrap();
    assert!(view["scope"].get("principal").is_none());
    assert!(view["origin"].get("principal").is_none());
    assert_eq!(view["revision"], 4);
    let _ = Adaptation {
        from_principal: "a".into(),
        from_domain: "d".into(),
        to_principal: "b".into(),
        to_domain: "d".into(),
        rewritten: vec!["x".into()],
        rationale: "r".into(),
    };
}
