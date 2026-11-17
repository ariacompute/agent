//! Tool execution pipeline: preset → approval → dispatch → post-process.
//!
//! The five segments are fixed and ordered (ADR-0016). Anything that cannot be
//! answered cleanly ends as a `ToolResult { is_error: true }` whose content tells
//! the model *why* — the loop never silently executes a denied call.

use super::approvals::{ApprovalBroker, ApprovalDecision, ApprovalRequest, DenyAll};
use super::policy::SandboxPolicy;
use super::SeamError;
use crate::context::ContextStore;
use crate::{Tool, ToolCall, ToolHandler, ToolResult};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;

/// Default cap for a single tool result fed back to the model (64 KiB).
pub const DEFAULT_MAX_RESULT_BYTES: usize = 64 * 1024;

/// Pointer to the externalised copy of a large tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpillRef {
    pub path: String,
    pub bytes: usize,
}

/// Result of the post-execute segment.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutcome {
    pub result: ToolResult,
    /// Present when an approval was actually consulted.
    pub approval: Option<ApprovalDecision>,
    /// Present when the result exceeded the cap and was externalised.
    pub spill: Option<SpillRef>,
}

/// Truncate + optionally externalise oversized results.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostExecPolicy {
    pub max_bytes: usize,
    pub spill_dir: Option<PathBuf>,
}

impl Default for PostExecPolicy {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_MAX_RESULT_BYTES,
            spill_dir: None,
        }
    }
}

impl PostExecPolicy {
    pub fn with_max_bytes(mut self, max: usize) -> Self {
        self.max_bytes = max;
        self
    }

    pub fn with_spill_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.spill_dir = Some(dir.into());
        self
    }

    /// Apply the cap. Truncation keeps the head (where tools usually print the
    /// discriminating information) and appends a retrieval pointer.
    pub fn apply(&self, call_id: &str, content: String) -> (String, Option<SpillRef>) {
        if content.len() <= self.max_bytes {
            return (content, None);
        }
        let mut cut = self.max_bytes;
        while cut > 0 && !content.is_char_boundary(cut) {
            cut -= 1;
        }
        let head = &content[..cut];
        let mut spill = None;
        if let Some(dir) = &self.spill_dir {
            let path = dir.join(format!("tool-{call_id}.txt"));
            if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&path, &content).is_ok() {
                spill = Some(SpillRef {
                    path: path.to_string_lossy().to_string(),
                    bytes: content.len(),
                });
            }
        }
        let pointer = match &spill {
            Some(s) => format!(
                "\n[truncated: kept {} of {} bytes; full output at {}]",
                cut,
                content.len(),
                s.path
            ),
            None => format!("\n[truncated: kept {} of {} bytes]", cut, content.len()),
        };
        (format!("{head}{pointer}"), spill)
    }
}

/// Outcome of the authorisation segment.
#[derive(Debug, Clone, PartialEq)]
pub enum Authz {
    Allowed {
        approval: Option<ApprovalDecision>,
    },
    Denied {
        decision: ApprovalDecision,
        reason: String,
    },
}

/// The fixed pipeline.
pub struct ToolExecutionPipeline {
    handlers: Vec<Arc<dyn ToolHandler>>,
    policy: Option<Arc<SandboxPolicy>>,
    approvals: Arc<dyn ApprovalBroker>,
    approvals_configured: bool,
    post: PostExecPolicy,
}

impl ToolExecutionPipeline {
    pub fn new(handlers: Vec<Arc<dyn ToolHandler>>) -> Self {
        Self {
            handlers,
            policy: None,
            approvals: Arc::new(DenyAll),
            approvals_configured: false,
            post: PostExecPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: Arc<SandboxPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    pub fn with_approvals(mut self, broker: Arc<dyn ApprovalBroker>) -> Self {
        self.approvals = broker;
        self.approvals_configured = true;
        self
    }

    pub fn with_post_exec(mut self, post: PostExecPolicy) -> Self {
        self.post = post;
        self
    }

    pub fn handlers(&self) -> &[Arc<dyn ToolHandler>] {
        &self.handlers
    }

    pub fn policy(&self) -> Option<&Arc<SandboxPolicy>> {
        self.policy.as_ref()
    }

    /// Whether a handler owns this tool (handlers win over the shell fallback).
    pub fn handler_for(&self, name: &str) -> Option<Arc<dyn ToolHandler>> {
        self.handlers.iter().find(|h| h.handles(name)).cloned()
    }

    /// Segment 2 + 3: preset gate, then approval.
    pub async fn authorize(&self, call: &ToolCall, session: &str) -> Authz {
        let preset = self.policy.as_ref().map(|p| p.preset()).unwrap_or_default();
        if let Some(policy) = &self.policy {
            if let Err(e) = policy.check_tool(&call.name) {
                return Authz::Denied {
                    decision: ApprovalDecision::Deny {
                        reason: e.to_string(),
                    },
                    reason: e.to_string(),
                };
            }
        }
        // Read-only calls need no round-trip; a trusted preset skips it too.
        if super::policy::is_read_only_tool(&call.name) || preset.auto_approves_mutation() {
            return Authz::Allowed { approval: None };
        }
        let req = ApprovalRequest::new(call.name.clone(), session, call.arguments.clone())
            .with_preset(preset);
        let decision = self.approvals.decide(&req).await;
        if decision.is_allowed() {
            Authz::Allowed {
                approval: Some(decision),
            }
        } else {
            let reason = if self.approvals_configured {
                decision.reason()
            } else {
                format!(
                    "{} (no approval provider configured; fail-closed)",
                    decision.reason()
                )
            };
            Authz::Denied { decision, reason }
        }
    }

    /// Segment 4: run the owning handler, if any.
    pub async fn dispatch(
        &self,
        call: &ToolCall,
        ctx: &Arc<dyn ContextStore>,
        session: &str,
    ) -> Option<ToolResult> {
        match self.handler_for(&call.name) {
            Some(handler) => Some(handler.run(call, ctx, session).await),
            None => None,
        }
    }

    /// Segment 5: cap the result and externalise the overflow.
    pub fn post_execute(
        &self,
        call_id: &str,
        result: ToolResult,
        approval: Option<ApprovalDecision>,
    ) -> ToolOutcome {
        let (content, spill) = self.post.apply(call_id, result.content);
        ToolOutcome {
            result: ToolResult { content, ..result },
            approval,
            spill,
        }
    }

    /// Build the `is_error` result for a denied call.
    pub fn denied_result(call: &ToolCall, reason: &str) -> ToolResult {
        ToolResult {
            call_id: call.id.clone(),
            content: format!("ERROR [denied]: {reason}"),
            is_error: true,
        }
    }

    /// Build the `is_error` result for a seam failure.
    pub fn seam_error_result(call_id: &str, err: &SeamError) -> ToolResult {
        ToolResult {
            call_id: call_id.to_string(),
            content: format!("ERROR [{}]: {err}", err.code()),
            is_error: true,
        }
    }
}

/// Build the JSON schema shared by read-only filesystem tools.
pub fn fs_tool_schema(properties: Value, required: &[&str]) -> Value {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Convenience: a tool definition with a JSON-schema object.
pub fn tool(name: &str, description: &str, parameters: Value) -> Tool {
    Tool {
        name: name.to_string(),
        description: description.to_string(),
        parameters,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::in_memory_context;
    use crate::seams::approvals::AutoAllow;
    use crate::seams::policy::PermissionPreset;
    use async_trait::async_trait;
    use serde_json::json;

    struct Echo {
        name: &'static str,
        reply: String,
    }

    #[async_trait]
    impl ToolHandler for Echo {
        fn handles(&self, name: &str) -> bool {
            name == self.name
        }

        async fn run(
            &self,
            call: &ToolCall,
            _ctx: &Arc<dyn ContextStore>,
            _session: &str,
        ) -> ToolResult {
            ToolResult {
                call_id: call.id.clone(),
                content: self.reply.clone(),
                is_error: false,
            }
        }
    }

    fn call(name: &str) -> ToolCall {
        ToolCall {
            id: "c1".into(),
            name: name.into(),
            arguments: json!({"path": "a.txt"}),
        }
    }

    fn policy(preset: PermissionPreset) -> Arc<SandboxPolicy> {
        Arc::new(SandboxPolicy::new(std::env::temp_dir(), preset).unwrap())
    }

    #[tokio::test]
    async fn read_only_tools_skip_approval() {
        let p = ToolExecutionPipeline::new(vec![]).with_policy(policy(PermissionPreset::ReadOnly));
        assert!(matches!(
            p.authorize(&call("fs_read"), "s").await,
            Authz::Allowed { approval: None }
        ));
    }

    #[tokio::test]
    async fn mutating_tools_without_broker_are_denied() {
        let p = ToolExecutionPipeline::new(vec![])
            .with_policy(policy(PermissionPreset::WorkspaceWrite));
        let authz = p.authorize(&call("shell"), "s").await;
        match authz {
            Authz::Denied { decision, reason } => {
                assert!(matches!(decision, ApprovalDecision::Unavailable { .. }));
                assert!(reason.contains("fail-closed"), "{reason}");
            }
            Authz::Allowed { .. } => panic!("must be denied"),
        }
    }

    #[tokio::test]
    async fn full_access_skips_approval_and_auto_allow_approves() {
        let p =
            ToolExecutionPipeline::new(vec![]).with_policy(policy(PermissionPreset::FullAccess));
        assert!(matches!(
            p.authorize(&call("shell"), "s").await,
            Authz::Allowed { approval: None }
        ));
        let p = ToolExecutionPipeline::new(vec![])
            .with_policy(policy(PermissionPreset::WorkspaceWrite))
            .with_approvals(Arc::new(AutoAllow));
        match p.authorize(&call("fs_write"), "s").await {
            Authz::Allowed { approval: Some(d) } => assert!(d.is_allowed()),
            other => panic!("expected allow, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn read_only_preset_denies_mutation_before_approval() {
        let p = ToolExecutionPipeline::new(vec![])
            .with_policy(policy(PermissionPreset::ReadOnly))
            .with_approvals(Arc::new(AutoAllow));
        assert!(matches!(
            p.authorize(&call("fs_write"), "s").await,
            Authz::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn dispatch_routes_to_owning_handler_only() {
        let handler: Arc<dyn ToolHandler> = Arc::new(Echo {
            name: "fs_read",
            reply: "contents".into(),
        });
        let p = ToolExecutionPipeline::new(vec![handler]);
        let ctx = in_memory_context();
        let out = p.dispatch(&call("fs_read"), &ctx, "s").await.unwrap();
        assert_eq!(out.content, "contents");
        assert!(p.dispatch(&call("shell"), &ctx, "s").await.is_none());
        assert!(p.handler_for("fs_read").is_some());
        assert!(p.handler_for("nope").is_none());
        assert_eq!(p.handlers().len(), 1);
        assert!(p.policy().is_none());
    }

    #[test]
    fn short_results_pass_through() {
        let p = ToolExecutionPipeline::new(vec![]);
        let out = p.post_execute(
            "c1",
            ToolResult {
                call_id: "c1".into(),
                content: "ok".into(),
                is_error: false,
            },
            None,
        );
        assert_eq!(out.result.content, "ok");
        assert!(out.spill.is_none());
    }

    #[test]
    fn long_results_truncate_with_pointer() {
        let p = ToolExecutionPipeline::new(vec![])
            .with_post_exec(PostExecPolicy::default().with_max_bytes(8));
        let out = p.post_execute(
            "c1",
            ToolResult {
                call_id: "c1".into(),
                content: "0123456789abcdef".into(),
                is_error: false,
            },
            None,
        );
        assert!(out.result.content.starts_with("01234567"));
        assert!(out.result.content.contains("[truncated"));
        assert!(out.spill.is_none());
    }

    #[test]
    fn spill_writes_the_full_output() {
        let dir = tempfile::tempdir().unwrap();
        let p = ToolExecutionPipeline::new(vec![]).with_post_exec(
            PostExecPolicy::default()
                .with_max_bytes(4)
                .with_spill_dir(dir.path()),
        );
        let full = "abcdefghij";
        let out = p.post_execute(
            "c9",
            ToolResult {
                call_id: "c9".into(),
                content: full.into(),
                is_error: false,
            },
            None,
        );
        let spill = out.spill.expect("spill ref");
        assert_eq!(spill.bytes, full.len());
        assert_eq!(std::fs::read_to_string(&spill.path).unwrap(), full);
        assert!(out.result.content.contains(&spill.path));
    }

    #[test]
    fn denied_and_seam_results_are_errors() {
        let call = call("shell");
        let denied = ToolExecutionPipeline::denied_result(&call, "nope");
        assert!(denied.is_error);
        assert!(denied.content.contains("denied"));
        let seam = ToolExecutionPipeline::seam_error_result(
            "c1",
            &SeamError::Unavailable("no web".into()),
        );
        assert!(seam.is_error);
        assert!(seam.content.contains("unavailable"));
    }

    #[test]
    fn helpers_build_tool_and_schema() {
        let t = tool("fs_read", "read", fs_tool_schema(json!({}), &["path"]));
        assert_eq!(t.name, "fs_read");
        assert_eq!(t.parameters["required"][0], "path");
    }
}
