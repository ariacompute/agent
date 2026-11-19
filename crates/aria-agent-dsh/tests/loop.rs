//! Offline tests for `DshAgentLoop`: a scripted runtime replaces the child
//! process, so the whole turn mapping runs without network, dsh or credentials.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use agent_core::context::RecallQuery;
use agent_core::seams::{AgentLoop, ToolExecutionPipeline};
use agent_core::session::FallbackOnly;
use agent_core::{
    in_memory_context, AgentConfig, AgentEvent, CoreError, ModelClient, ModelRequest, ModelResponse,
};
use agent_sandbox::{ExecOutput, ExecSpec, Sandbox, SandboxError, SandboxHandle};
use aria_agent_dsh::{
    no_bridge, DshAgentLoop, DshConfig, DshPool, HarnessRuntime, RuntimeConnector,
    ServerRequestHandler,
};
use async_trait::async_trait;
use serde_json::{json, Value};

struct NoSandbox;

#[async_trait]
impl Sandbox for NoSandbox {
    async fn spawn(&self, _spec: &ExecSpec) -> Result<SandboxHandle, SandboxError> {
        Ok(SandboxHandle { id: "noop".into() })
    }

    async fn exec(
        &self,
        _handle: &SandboxHandle,
        _cmd: &[String],
    ) -> Result<ExecOutput, SandboxError> {
        Ok(ExecOutput {
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        })
    }

    async fn destroy(&self, _handle: SandboxHandle) -> Result<(), SandboxError> {
        Ok(())
    }
}

struct SilentModel;

#[async_trait]
impl ModelClient for SilentModel {
    async fn complete(&self, req: &ModelRequest) -> Result<ModelResponse, CoreError> {
        Ok(ModelResponse {
            text: format!("native: {}", req.input),
        })
    }
}

fn context(session: &str) -> agent_core::seams::LoopContext {
    let config = AgentConfig {
        session: session.into(),
        ..AgentConfig::default()
    };
    agent_core::seams::LoopContext {
        prompt: agent_core::build_prompt(&config),
        model: Arc::new(SilentModel),
        context: in_memory_context(),
        sandbox: Arc::new(NoSandbox),
        pipeline: Arc::new(ToolExecutionPipeline::new(Vec::new())),
        session_log: None,
        tools: Vec::new(),
        title_pending: Arc::new(AtomicBool::new(true)),
        title_provider: Arc::new(FallbackOnly),
        config,
    }
}

/// Runtime that replays a fixed script, then closes the channel.
struct Scripted {
    script: Vec<Value>,
}

#[async_trait]
impl HarnessRuntime for Scripted {
    async fn prompt(
        &self,
        _session: &str,
        _input: &str,
    ) -> Result<tokio::sync::mpsc::Receiver<Value>, CoreError> {
        let (tx, rx) = tokio::sync::mpsc::channel(self.script.len() + 1);
        for frame in &self.script {
            tx.send(frame.clone())
                .await
                .map_err(|e| CoreError::Model(format!("scripted runtime closed: {e}")))?;
        }
        Ok(rx)
    }

    async fn shutdown(&self) -> Result<(), CoreError> {
        Ok(())
    }
}

struct ScriptedConnector {
    script: Vec<Value>,
}

#[async_trait]
impl RuntimeConnector for ScriptedConnector {
    async fn connect(
        &self,
        _cfg: DshConfig,
        _handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Arc<dyn HarnessRuntime>, CoreError> {
        Ok(Arc::new(Scripted {
            script: self.script.clone(),
        }))
    }
}

struct FailingConnector;

#[async_trait]
impl RuntimeConnector for FailingConnector {
    async fn connect(
        &self,
        _cfg: DshConfig,
        _handler: Arc<dyn ServerRequestHandler>,
    ) -> Result<Arc<dyn HarnessRuntime>, CoreError> {
        Err(CoreError::Config(
            "cannot start DeepSeek Harness `dsh`: not found".into(),
        ))
    }
}

fn harness(script: Vec<Value>) -> DshAgentLoop {
    let cfg = DshConfig::for_session(&PathBuf::from("/tmp/aria-dsh-test"), "s1", "m");
    let pool = Arc::new(DshPool::with_connector(
        cfg,
        2,
        Duration::from_secs(60),
        no_bridge(),
        Arc::new(ScriptedConnector { script }),
    ));
    DshAgentLoop::new(pool)
}

fn frame(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

async fn collect(
    harness: &DshAgentLoop,
    ctx: agent_core::seams::LoopContext,
    input: &str,
) -> Vec<Result<AgentEvent, CoreError>> {
    let mut stream = harness.run(ctx, input).await.expect("stream starts");
    let mut out = Vec::new();
    while let Some(item) = futures::StreamExt::next(&mut stream).await {
        out.push(item);
    }
    out
}

#[tokio::test]
async fn a_completed_turn_maps_to_the_native_event_vocabulary() {
    let loop_ = harness(vec![
        frame("turn/start", json!({})),
        frame("step/start", json!({})),
        frame("tool/call", json!({"call": {"id": "c1", "name": "shell"}})),
        frame(
            "tool/result",
            json!({"call": {"id": "c1", "name": "shell", "arguments": {"command": ["ls"]}},
                   "result": {"content": "exit=0"}}),
        ),
        frame(
            "assistant/message",
            json!({"content": "done", "usage": {"inputTokens": 5, "outputTokens": 2, "model": "deepseek-v4"}}),
        ),
        frame("turn/end", json!({"reason": {"kind": "completed"}})),
    ]);
    let ctx = context("s1");
    let store = ctx.context.clone();
    let events = collect(&loop_, ctx, "hi").await;
    let mapped: Vec<&AgentEvent> = events
        .iter()
        .map(|e| e.as_ref().expect("no error"))
        .collect();
    assert!(matches!(mapped[0], AgentEvent::Step { phase, .. } if phase == "harness"));
    assert!(matches!(mapped[1], AgentEvent::Step { phase, .. } if phase == "step"));
    assert!(matches!(
        mapped[2],
        AgentEvent::Step { phase, label } if phase == "tool_exec" && label.as_deref() == Some("shell")
    ));
    assert!(
        matches!(mapped[3], AgentEvent::ToolCall { name, result, .. } if name == "shell" && result.content == "exit=0")
    );
    assert!(matches!(
        mapped[4],
        AgentEvent::Usage {
            input_tokens: 5,
            output_tokens: 2,
            ..
        }
    ));
    assert!(matches!(mapped[5], AgentEvent::Token { text } if text == "done"));
    assert!(matches!(mapped[6], AgentEvent::Done { text } if text == "done"));

    // The assistant text is remembered exactly like the native loop does.
    let fragments = store
        .recall(&RecallQuery::new("s1", "done"))
        .await
        .expect("recall");
    assert!(fragments.iter().any(|f| f.content == "done"));
}

#[tokio::test]
async fn a_failed_turn_and_a_silent_runtime_both_fail_the_turn() {
    let failed = harness(vec![frame(
        "turn/end",
        json!({"reason": {"kind": "error"}}),
    )]);
    let events = collect(&failed, context("s1"), "hi").await;
    assert!(matches!(events.last(), Some(Err(CoreError::Model(_)))));

    // No terminal frame at all: the runtime died mid-turn.
    let silent = harness(vec![frame("turn/start", json!({}))]);
    let events = collect(&silent, context("s2"), "hi").await;
    assert!(matches!(
        events.last(),
        Some(Err(CoreError::Model(message))) if message.contains("without `turn/end`")
    ));
}

#[tokio::test]
async fn protocol_violations_and_errors_are_surfaced() {
    let broken = harness(vec![
        frame("turn/end", json!({"reason": {}})),
        frame("turn/start", json!({})),
    ]);
    let events = collect(&broken, context("s1"), "hi").await;
    assert!(matches!(
        events.last(),
        Some(Err(CoreError::Model(message))) if message.contains("protocol error")
    ));

    let error = harness(vec![frame("error", json!({"message": "quota exceeded"}))]);
    let events = collect(&error, context("s1"), "hi").await;
    assert!(matches!(
        events.last(),
        Some(Err(CoreError::Model(message))) if message.contains("quota exceeded")
    ));
}

#[tokio::test]
async fn a_runtime_that_cannot_start_fails_before_the_stream() {
    let cfg = DshConfig::for_session(&PathBuf::from("/tmp/aria-dsh-test"), "s1", "m");
    let pool = Arc::new(DshPool::with_connector(
        cfg,
        1,
        Duration::from_secs(60),
        no_bridge(),
        Arc::new(FailingConnector),
    ));
    let err = DshAgentLoop::new(pool)
        .run(context("s1"), "hi")
        .await
        .err()
        .expect("start must fail closed");
    assert!(
        err.to_string().contains("cannot start DeepSeek Harness"),
        "{err}"
    );
}

#[tokio::test]
async fn the_loop_exposes_its_pool() {
    let loop_ = harness(Vec::new());
    assert_eq!(loop_.pool().len().await, 0);
}

#[tokio::test]
async fn a_turn_without_assistant_text_writes_no_memory() {
    let loop_ = harness(vec![frame(
        "turn/end",
        json!({"reason": {"kind": "max-tokens"}}),
    )]);
    let ctx = context("empty-turn");
    let store = ctx.context.clone();
    let events = collect(&loop_, ctx, "hi").await;
    assert!(matches!(events.last(), Some(Ok(AgentEvent::Done { text })) if text.is_empty()));
    let fragments = store
        .recall(&RecallQuery::new("empty-turn", "hi"))
        .await
        .expect("recall");
    // Only the user turn is remembered: an empty assistant answer is not a memory.
    assert_eq!(fragments.len(), 1);
    assert_eq!(fragments[0].content, "hi");
}

#[tokio::test]
async fn graph_memory_stays_on_for_harness_turns() {
    let loop_ = harness(vec![
        frame("assistant/message", json!({"content": "remember me"})),
        frame("turn/end", json!({"reason": {"kind": "completed"}})),
    ]);
    let mut ctx = context("graph-turn");
    ctx.config.memory_graph = true;
    let store = ctx.context.clone();
    let events = collect(&loop_, ctx, "hi").await;
    assert!(matches!(events.last(), Some(Ok(AgentEvent::Done { .. }))));
    let fragments = store
        .recall(&RecallQuery::new("graph-turn", "remember me"))
        .await
        .expect("recall");
    assert!(fragments.iter().any(|f| f.content == "remember me"));
}
