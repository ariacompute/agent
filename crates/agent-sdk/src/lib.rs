//! `agent-sdk` — the stable UniFFI boundary.
//!
//! This crate compiles to a native `cdylib` (`libagent_sdk`) that Swift and
//! Kotlin consume through generated bindings (see `just ffi`). It wraps
//! `agent-core` (the codex-harness runtime) and `agent-memo`, exposing a small,
//! stable surface: [`SdkAgent`] and [`SdkSession`].
//!
//! FFI boundary stability rule (docs/adr/0002-ffi-boundary.md): any change here
//! must be followed by regenerating the Swift/Kotlin bindings and a CI check.

uniffi::setup_scaffolding!();

use agent_core::{Agent, AgentConfig};
use agent_memo::{ContextFragment, FragmentKind, MemoStore, SledMemoStore};
use std::sync::{Arc, OnceLock};
use tokio::runtime::Runtime;

fn rt() -> &'static Runtime {
    static RT: OnceLock<Runtime> = OnceLock::new();
    RT.get_or_init(|| Runtime::new().expect("tokio runtime"))
}

/// SDK-side agent configuration (mirrors `agent_core::AgentConfig`).
#[derive(Clone, Debug, uniffi::Record)]
pub struct SdkAgentConfig {
    pub session: String,
    pub agent_name: String,
    pub sandbox_provider: String,
    pub model: String,
}

impl Default for SdkAgentConfig {
    fn default() -> Self {
        Self {
            session: "default".to_string(),
            agent_name: "agent".to_string(),
            sandbox_provider: "docker".to_string(),
            model: "gpt-4o-mini".to_string(),
        }
    }
}

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum SdkError {
    #[error("core error: {0}")]
    Core(String),
}

/// A native agent handle. Construct via [`create_agent`].
#[derive(uniffi::Object)]
pub struct SdkAgent {
    inner: Arc<Agent>,
}

#[uniffi::export]
impl SdkAgent {
    /// Run a single turn and return the assistant reply.
    pub fn run(&self, input: String) -> Result<String, SdkError> {
        rt().block_on(self.inner.run(&input))
            .map_err(|e| SdkError::Core(e.to_string()))
    }

    /// Get the session memory handle for this agent.
    pub fn session(&self) -> Arc<SdkSession> {
        Arc::new(SdkSession {
            memo: self.inner.memo(),
            session: self.inner.session().to_string(),
        })
    }
}

/// A session's memo handle (long-term / externalized memory).
#[derive(uniffi::Object)]
pub struct SdkSession {
    memo: Arc<dyn MemoStore>,
    session: String,
}

#[uniffi::export]
impl SdkSession {
    /// Write a long-term memory under `key`.
    pub fn memorize(&self, key: String, value: String) -> Result<(), SdkError> {
        let frag = ContextFragment::new(&self.session, FragmentKind::LongTerm, value).with_key(key);
        rt().block_on(self.memo.memorize(frag))
            .map_err(|e| SdkError::Core(e.to_string()))
    }

    /// Read a long-term memory by `key`.
    pub fn recall(&self, key: String) -> Result<Option<String>, SdkError> {
        let frag = rt()
            .block_on(self.memo.get_by_key(&self.session, &key))
            .map_err(|e| SdkError::Core(e.to_string()))?;
        Ok(frag.map(|f| f.content))
    }
}

/// Build an agent with an in-memory memo store.
#[uniffi::export]
pub fn create_agent(config: SdkAgentConfig) -> Result<Arc<SdkAgent>, SdkError> {
    let memo = SledMemoStore::memory().map_err(|e| SdkError::Core(e.to_string()))?;
    let cfg = AgentConfig {
        session: config.session,
        agent_name: config.agent_name,
        sandbox_provider: config.sandbox_provider,
        model: config.model,
    };
    let agent = Agent::new(cfg, memo).map_err(|e| SdkError::Core(e.to_string()))?;
    Ok(Arc::new(SdkAgent {
        inner: Arc::new(agent),
    }))
}
