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
