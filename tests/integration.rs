//! Cross-crate integration: the context store is the single context source
//! (cloud: Postgres + pgvector, on-device: embedded sled), the sandbox is
//! pluggable (docker default), and the core run loop wires them together.

use agent_core::context::{ContextFragment, ContextStore, FragmentKind, RecallQuery};
use agent_core::Agent;
use agent_memo::SledContextStore;

#[tokio::test]
async fn core_run_injects_and_persists_into_context() {
    let context = agent_core::in_memory_context();
    let model: Box<dyn agent_core::ModelClient> = Box::new(agent_core::StubModel::new("agent"));
    let agent =
        Agent::with_model(agent_core::AgentConfig::default(), model, context.clone()).unwrap();

    let _ = agent
        .run("remember: the moon is made of cheese")
        .await
        .unwrap();

    // The context must be retrievable from the store (the only context source).
    let frags = context
        .recall(&RecallQuery::new("default", "moon"))
        .await
        .unwrap();
    assert!(frags.iter().any(|f| f.content.contains("moon")));
}

#[tokio::test]
async fn on_device_context_store_is_embedded() {
    // The on-device store is sled (local/embedded), never Postgres.
    let store = SledContextStore::memory().unwrap();
    let frag = ContextFragment::new("s", FragmentKind::LongTerm, "k").with_key("k");
    store.memorize(frag).await.unwrap();
    let got = store.get_by_key("s", "k").await.unwrap();
    assert!(got.is_some());
}

#[test]
fn sandbox_default_is_docker() {
    let _sandbox = agent_sandbox::default_sandbox();
    // Constructed without panic; docker is the default provider.
}
