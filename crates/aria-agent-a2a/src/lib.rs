//! `aria-agent-a2a` — the [Agent2Agent] wire contract, as bytes.
//!
//! This crate is the **only** place that decides what an A2A frame looks like.
//! It performs no IO and holds no state: the cloud crate projects its own
//! `AgentEvent` stream onto these types, and the tests pin the encoding.
//!
//! Three compatibility notes drive the shape of everything here, because the
//! first client is agenthub (`agenthub/internal/a2a` + `adapter/agent.go`):
//!
//! * a [`Part`] is emitted with **both** `kind` (spec) and `type` (agenthub's
//!   own wire format). A part that carries only `kind` is decoded as text-less
//!   by that client, so the duplication is load-bearing, not cosmetic;
//! * a streaming envelope always carries `kind`, and the status/artifact
//!   envelopes additionally carry the legacy `event` name (`task_status_update`
//!   / `task_artifact_update`) that some clients dispatch on;
//! * the stream has **no sentinel**: a run ends with the `final: true` frame
//!   (`completed` / `failed` / `canceled`) or with `input-required`.
//!
//! [Agent2Agent]: https://a2a-protocol.org

pub mod card;
pub mod jsonrpc;
pub mod state;
pub mod stream;
pub mod time;
pub mod types;

pub use card::{AgentCard, AgentSkill, Authentication, Capabilities, Provider, PROTOCOL_VERSION};
pub use jsonrpc::{code, Method, Request, Response, RpcError};
pub use state::TaskState;
pub use stream::{encode, event_name, sse_frame};
pub use types::{
    Artifact, FileContent, Message, MessageSendConfiguration, MessageSendParams, Part,
    PushGetParams, PushNotificationConfig, PushSetParams, Role, StreamFrame, Task,
    TaskArtifactUpdateEvent, TaskQueryParams, TaskResumeParams, TaskStatus, TaskStatusUpdateEvent,
};
