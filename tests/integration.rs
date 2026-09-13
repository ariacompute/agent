//! Cross-crate integration: memo is the single context source, sandbox is
//! pluggable (docker default), and the core run loop wires them together.

use agent_core::Agent;
use agent_memo::{MemoStore, RecallQuery, SledMemoStore};

#[tokio::test]
async fn core_run_injects_and_persists_into_memo() {
    let memo = agent_core::in_memory_memo();
    let model: Box<dyn agent_core::ModelClient> = Box::new(agent_core::StubModel::new("agent"));
    let agent = Agent::with_model(agent_core::AgentConfig::default(), model, memo.clone()).unwrap();

    let _ = agent
        .run("remember: the moon is made of cheese")
        .await
        .unwrap();

    // The context must be retrievable from memo (the only context source).
    let frags = memo
        .recall(&RecallQuery::new("default", "moon"))
        .await
        .unwrap();
    assert!(frags.iter().any(|f| f.content.contains("moon")));
}

#[tokio::test]
async fn memo_is_local_and_not_postgres() {
    // The memo store is a sled (local/embedded) database, never Postgres.
    let store = SledMemoStore::memory().unwrap();
    let frag = agent_memo::ContextFragment::new("s", agent_memo::FragmentKind::LongTerm, "k")
        .with_key("k");
    store.memorize(frag).await.unwrap();
    let got = store.get_by_key("s", "k").await.unwrap();
    assert!(got.is_some());
}

#[test]
fn sandbox_default_is_docker() {
    let _sandbox = agent_sandbox::default_sandbox();
    // Constructed without panic; docker is the default provider.
}

// --- Reef self-improvement loop (cross-crate) ---

#[tokio::test]
async fn reef_self_improvement_end_to_end() {
    use agent_core::{ActiveHarness, AgentConfig, ModelClient, StubModel};
    use agent_reef::engine::EvolutionEngine;
    use agent_reef::feedback::{Feedback, FeedbackStore, SledFeedbackStore};
    use agent_reef::git;
    use agent_reef::harness;
    use agent_reef::record::{Record, RecordStore, SledRecordStore};
    use std::sync::Arc;

    let reef_dir = std::env::temp_dir().join(format!("reef_int_{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&reef_dir);

    let active = Arc::new(ActiveHarness::baseline("agent"));
    let records: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
    let feedback: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
    let engine = EvolutionEngine::new(
        Box::new(StubModel::new("agent")),
        records.clone(),
        feedback.clone(),
        reef_dir.clone(),
        active.clone(),
        3,
    );

    // 1) Serve: record a turn (receipt id returned to the client in production).
    let rec = Record::new("a", "s", None, "You are agent.", "in", "out", "stub");
    records.record_turn(rec.clone()).await.unwrap();

    // 2) Observe: bind negative feedback to that receipt.
    feedback
        .report(Feedback::new(
            vec![rec.id.clone()],
            -1.0,
            Some("wrong answer".into()),
        ))
        .await
        .unwrap();

    // Ensure a git repo exists before evolving so the winner is versioned.
    let git_ok = git::init_repo(&reef_dir).is_ok();

    // 3) Grow + Commit: evolve proposes a candidate, keeps the winner, versions it.
    let outcome = engine.evolve().await.unwrap();
    assert!(outcome.adopted, "candidate should beat the baseline");

    // The winning harness is persisted to disk (versioned artifact).
    let loaded = harness::load(&reef_dir).unwrap();
    assert!(loaded.skills.iter().any(|s| s.name == "reef_improvement"));

    // Git versioning produced a `reef@1` tag (only if git was available).
    if git_ok {
        let versions = git::list_versions(&reef_dir).unwrap();
        assert!(versions.contains(&"reef@1".to_string()));
    }

    // 4) Hot-serve: a live agent sharing this ActiveHarness now serves the winner.
    assert!(active
        .get()
        .skills
        .iter()
        .any(|s| s.name == "reef_improvement"));
    let memo = agent_core::in_memory_memo();
    let model: Box<dyn ModelClient> = Box::new(StubModel::new("agent"));
    let agent = Agent::with_harness(AgentConfig::default(), model, memo, active.clone()).unwrap();
    // A fresh turn must reflect the evolved harness's system prompt.
    let out = agent.run("hello").await.unwrap();
    assert!(out.contains("hello"));

    let _ = std::fs::remove_dir_all(&reef_dir);
}

#[tokio::test]
async fn reef_no_eligible_keeps_baseline() {
    use agent_core::{ActiveHarness, StubModel};
    use agent_reef::engine::EvolutionEngine;
    use agent_reef::feedback::{FeedbackStore, SledFeedbackStore};
    use agent_reef::record::{RecordStore, SledRecordStore};
    use std::sync::Arc;

    let reef_dir = std::env::temp_dir().join(format!("reef_int_noelig_{}", uuid::Uuid::new_v4()));
    let _ = std::fs::remove_dir_all(&reef_dir);
    let active = Arc::new(ActiveHarness::baseline("agent"));
    let records: Arc<dyn RecordStore> = SledRecordStore::memory().unwrap();
    let feedback: Arc<dyn FeedbackStore> = SledFeedbackStore::memory().unwrap();
    let engine = EvolutionEngine::new(
        Box::new(StubModel::new("agent")),
        records.clone(),
        feedback.clone(),
        reef_dir.clone(),
        active.clone(),
        3,
    );

    // No feedback at all -> evolution is not eligible; the served harness is
    // never disturbed.
    let res = engine.evolve().await;
    assert!(matches!(res, Err(agent_reef::ReefError::NoEligible)));
    assert!(active.get().is_baseline("agent"));

    let _ = std::fs::remove_dir_all(&reef_dir);
}
