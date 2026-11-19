//! Bridge dsh's server→client requests onto the approval / question seams.
//!
//! dsh asks the host before a mutating tool runs (`approval/request`) and when a
//! tool needs a human answer (`ask_user_question`). Both are answered through
//! the seams the deployment already installed, so a harness turn is exactly as
//! strict as a native one:
//!
//! * no approval provider ⇒ `Unavailable` ⇒ we answer **deny** with the reason
//!   (ADR-0016 fail-closed), never "allow";
//! * no question provider ⇒ JSON-RPC error, so the tool fails closed instead of
//!   receiving an empty string that looks like an answer.

use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use agent_core::seams::{ApprovalBroker, ApprovalRequest, PermissionPreset, UserQuestions};

use crate::rpc::{ServerRequestHandler, CODE_METHOD_NOT_FOUND};

/// JSON-RPC "invalid params".
pub const CODE_INVALID_PARAMS: i64 = -32602;

/// Answers dsh's approval / question requests from the installed seams.
pub struct SeamApprovalBridge {
    approvals: Arc<dyn ApprovalBroker>,
    questions: Arc<dyn UserQuestions>,
    session: String,
    preset: PermissionPreset,
}

impl SeamApprovalBridge {
    pub fn new(
        approvals: Arc<dyn ApprovalBroker>,
        questions: Arc<dyn UserQuestions>,
        session: impl Into<String>,
        preset: PermissionPreset,
    ) -> Self {
        Self {
            approvals,
            questions,
            session: session.into(),
            preset,
        }
    }
}

#[async_trait]
impl ServerRequestHandler for SeamApprovalBridge {
    async fn handle(&self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            crate::rpc::METHOD_APPROVAL_REQUEST => {
                let tool = params.get("tool").and_then(Value::as_str).ok_or_else(|| {
                    (
                        CODE_INVALID_PARAMS,
                        "approval request without `tool`".into(),
                    )
                })?;
                let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);
                // A pooled runtime serves several sessions: prefer the session
                // dsh names in the request, fall back to the bridge default.
                let session = params
                    .get("sessionId")
                    .and_then(Value::as_str)
                    .map(str::to_string)
                    .unwrap_or_else(|| self.session.clone());
                let request =
                    ApprovalRequest::new(tool, session, arguments).with_preset(self.preset);
                let decision = self.approvals.decide(&request).await;
                Ok(if decision.is_allowed() {
                    json!({"decision": "allow"})
                } else {
                    json!({"decision": "deny", "reason": decision.reason()})
                })
            }
            crate::rpc::METHOD_USER_QUESTION => {
                let question = params
                    .get("question")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        (
                            CODE_INVALID_PARAMS,
                            "question request without `question`".into(),
                        )
                    })?;
                let options: Vec<String> = params
                    .get("options")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                match self.questions.ask(question, &options).await {
                    Ok(answer) => Ok(json!({"answer": answer})),
                    Err(e) => Err((CODE_METHOD_NOT_FOUND, e.to_string())),
                }
            }
            other => Err((
                CODE_METHOD_NOT_FOUND,
                format!("unsupported dsh request: {other}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_core::seams::{AutoAllow, DenyAll, NoQuestions};

    #[tokio::test]
    async fn deny_all_is_reported_as_deny() {
        let bridge = SeamApprovalBridge::new(
            Arc::new(DenyAll),
            Arc::new(NoQuestions),
            "s1",
            PermissionPreset::WorkspaceWrite,
        );
        let answer = bridge
            .handle(
                crate::rpc::METHOD_APPROVAL_REQUEST,
                &json!({"tool": "shell", "arguments": {"command": ["rm", "-rf", "/"]}}),
            )
            .await
            .expect("handled");
        assert_eq!(answer["decision"], "deny");
        assert!(answer["reason"]
            .as_str()
            .unwrap_or("")
            .contains("unavailable"));
    }

    #[tokio::test]
    async fn an_installed_broker_can_allow() {
        let bridge = SeamApprovalBridge::new(
            Arc::new(AutoAllow),
            Arc::new(NoQuestions),
            "s1",
            PermissionPreset::FullAccess,
        );
        let answer = bridge
            .handle(
                crate::rpc::METHOD_APPROVAL_REQUEST,
                &json!({"tool": "fs_write"}),
            )
            .await
            .expect("handled");
        assert_eq!(answer, json!({"decision": "allow"}));
        assert!(bridge
            .handle(crate::rpc::METHOD_APPROVAL_REQUEST, &json!({}))
            .await
            .is_err());
    }

    struct Answering;

    #[async_trait]
    impl UserQuestions for Answering {
        async fn ask(
            &self,
            _question: &str,
            options: &[String],
        ) -> Result<String, agent_core::seams::SeamError> {
            Ok(options
                .first()
                .cloned()
                .unwrap_or_else(|| "free form".into()))
        }
    }

    #[tokio::test]
    async fn questions_use_the_installed_provider_and_fail_closed_without_one() {
        let answering = SeamApprovalBridge::new(
            Arc::new(AutoAllow),
            Arc::new(Answering),
            "s1",
            PermissionPreset::WorkspaceWrite,
        );
        let answer = answering
            .handle(
                crate::rpc::METHOD_USER_QUESTION,
                &json!({"question": "which env?", "options": ["dev", "prod"]}),
            )
            .await
            .expect("handled");
        assert_eq!(answer["answer"], "dev");

        // No options: the provider still gets a chance to answer freely.
        let free = answering
            .handle(
                crate::rpc::METHOD_USER_QUESTION,
                &json!({"question": "anything else?"}),
            )
            .await
            .expect("handled");
        assert_eq!(free["answer"], "free form");

        let headless = SeamApprovalBridge::new(
            Arc::new(AutoAllow),
            Arc::new(NoQuestions),
            "s1",
            PermissionPreset::WorkspaceWrite,
        );
        let err = headless
            .handle(
                crate::rpc::METHOD_USER_QUESTION,
                &json!({"question": "which env?"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.0, CODE_METHOD_NOT_FOUND);
        assert!(err.1.contains("unavailable"), "{err:?}");

        // Malformed requests and unknown methods are refused.
        assert_eq!(
            headless
                .handle(crate::rpc::METHOD_USER_QUESTION, &json!({}))
                .await
                .unwrap_err()
                .0,
            CODE_INVALID_PARAMS
        );
        assert_eq!(
            headless.handle("fs/read", &json!({})).await.unwrap_err().0,
            CODE_METHOD_NOT_FOUND
        );
    }
}
