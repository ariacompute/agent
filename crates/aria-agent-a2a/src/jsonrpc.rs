//! JSON-RPC 2.0 envelopes, A2A method names and the error-code catalogue.
//!
//! The catalogue is shared with the cloud crate so that a `code` written into a
//! response and the `data.code` string in a log line cannot drift apart.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// The only JSON-RPC version this server speaks.
pub const JSONRPC_VERSION: &str = "2.0";

/// A2A error codes (JSON-RPC reserved range + the A2A-specific block).
pub mod code {
    pub const PARSE_ERROR: i64 = -32700;
    pub const INVALID_REQUEST: i64 = -32600;
    pub const METHOD_NOT_FOUND: i64 = -32601;
    pub const INVALID_PARAMS: i64 = -32602;
    pub const INTERNAL_ERROR: i64 = -32603;
    pub const TASK_NOT_FOUND: i64 = -32001;
    pub const TASK_NOT_CANCELABLE: i64 = -32002;
    /// Reserved: this server never refuses push on a capability basis.
    pub const PUSH_NOT_SUPPORTED: i64 = -32003;
    pub const AGENT_NOT_FOUND: i64 = -32004;
    pub const INVALID_STATE_TRANSITION: i64 = -32005;
    pub const RESOURCE_NOT_FOUND: i64 = -32006;
    pub const TASK_NOT_RESUMABLE: i64 = -32008;
    /// Reserved: "unsupported operation" for a future method split.
    pub const UNSUPPORTED_OPERATION: i64 = -32009;
}

/// The stable machine name of a code (`task_not_found`), or `None` for an
/// unknown code. Log lines and `error.data.code` use this, never the number.
pub fn code_name(code: i64) -> Option<&'static str> {
    Some(match code {
        code::PARSE_ERROR => "parse_error",
        code::INVALID_REQUEST => "invalid_request",
        code::METHOD_NOT_FOUND => "method_not_found",
        code::INVALID_PARAMS => "invalid_params",
        code::INTERNAL_ERROR => "internal_error",
        code::TASK_NOT_FOUND => "task_not_found",
        code::TASK_NOT_CANCELABLE => "task_not_cancelable",
        code::PUSH_NOT_SUPPORTED => "push_not_supported",
        code::AGENT_NOT_FOUND => "agent_not_found",
        code::INVALID_STATE_TRANSITION => "invalid_state_transition",
        code::RESOURCE_NOT_FOUND => "resource_not_found",
        code::TASK_NOT_RESUMABLE => "task_not_resumable",
        code::UNSUPPORTED_OPERATION => "unsupported_operation",
        _ => return None,
    })
}

/// An inbound JSON-RPC request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Request {
    #[serde(default = "version")]
    pub jsonrpc: String,
    /// Absent for a notification; echoed back on the response when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

fn version() -> String {
    JSONRPC_VERSION.to_string()
}

/// An outbound JSON-RPC response (exactly one of `result` / `error`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Response {
    #[serde(default = "version")]
    pub jsonrpc: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// A successful response.
    pub fn ok(id: Option<Value>, result: Value) -> Self {
        Self {
            jsonrpc: version(),
            id,
            result: Some(result),
            error: None,
        }
    }

    /// A failed response carrying `error`.
    pub fn failed(id: Option<Value>, error: RpcError) -> Self {
        Self {
            jsonrpc: version(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

/// The error object of a failed response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RpcError {
    pub code: i64,
    pub message: String,
    /// `{"code": "task_not_found"}` for machines, when the code is known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// An error with the machine name folded into `data.code`.
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        let data = code_name(code).map(|name| serde_json::json!({ "code": name }));
        Self {
            code,
            message: message.into(),
            data,
        }
    }

    /// An error with caller-supplied `data` (overrides the machine name).
    pub fn with_data(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
            data: Some(data),
        }
    }
}

/// The methods this server implements, canonicalised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    MessageSend,
    MessageStream,
    TasksGet,
    TasksCancel,
    TasksResume,
    PushSet,
    PushGet,
}

impl Method {
    /// Parse a method name, accepting the v0.1 aliases unconditionally.
    pub fn parse(name: &str) -> Option<Self> {
        Some(match name.trim() {
            "message/send" | "tasks/send" => Method::MessageSend,
            "message/stream" | "tasks/sendSubscribe" => Method::MessageStream,
            "tasks/get" => Method::TasksGet,
            "tasks/cancel" => Method::TasksCancel,
            "tasks/resume" => Method::TasksResume,
            "tasks/pushNotification/set" => Method::PushSet,
            "tasks/pushNotification/get" => Method::PushGet,
            _ => return None,
        })
    }

    /// The canonical v1.0 name.
    pub fn name(self) -> &'static str {
        match self {
            Method::MessageSend => "message/send",
            Method::MessageStream => "message/stream",
            Method::TasksGet => "tasks/get",
            Method::TasksCancel => "tasks/cancel",
            Method::TasksResume => "tasks/resume",
            Method::PushSet => "tasks/pushNotification/set",
            Method::PushGet => "tasks/pushNotification/get",
        }
    }

    /// Whether the method mutates state and therefore needs the `write` scope.
    pub fn needs_write(self) -> bool {
        matches!(
            self,
            Method::MessageSend
                | Method::MessageStream
                | Method::TasksCancel
                | Method::TasksResume
                | Method::PushSet
        )
    }
}
