//! The A2A data model: parts, messages, tasks, artifacts and method payloads.
//!
//! Everything here is plain data: no clock, no IO, no ids are generated. The
//! cloud crate decides *when* to create a task; this module decides what it
//! looks like once created.

use crate::state::TaskState;
use crate::stream::event_name;
use serde::de::{self, Deserializer};
use serde::ser::Serializer;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Discriminator of a text [`Part`].
pub const PART_TEXT: &str = "text";
/// Discriminator of a file [`Part`].
pub const PART_FILE: &str = "file";
/// Discriminator of a structured-data [`Part`].
pub const PART_DATA: &str = "data";

/// The speaker of a [`Message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Role {
    User,
    Agent,
    /// A role this crate does not model; the wire string is preserved so a
    /// round-trip cannot silently rewrite it.
    Other(String),
}

impl Role {
    pub fn as_str(&self) -> &str {
        match self {
            Role::User => "user",
            Role::Agent => "agent",
            Role::Other(raw) => raw,
        }
    }
}

impl Serialize for Role {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Role {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        // Compare before moving: `match raw.as_str()` would keep `raw` borrowed.
        if raw == "user" {
            Ok(Role::User)
        } else if raw == "agent" {
            Ok(Role::Agent)
        } else {
            Ok(Role::Other(raw))
        }
    }
}

/// The payload of a file part: inline base64 `bytes`, a `uri`, or both.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bytes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl FileContent {
    fn to_json(&self) -> Value {
        let mut map = Map::new();
        if let Some(name) = &self.name {
            map.insert("name".into(), Value::String(name.clone()));
        }
        if let Some(mime_type) = &self.mime_type {
            map.insert("mimeType".into(), Value::String(mime_type.clone()));
        }
        if let Some(bytes) = &self.bytes {
            map.insert("bytes".into(), Value::String(bytes.clone()));
        }
        if let Some(uri) = &self.uri {
            map.insert("uri".into(), Value::String(uri.clone()));
        }
        if let Some(metadata) = &self.metadata {
            map.insert("metadata".into(), metadata.clone());
        }
        Value::Object(map)
    }
}

/// One piece of a [`Message`] or [`Artifact`].
///
/// Serialization writes **both** discriminators (`kind` and `type`) because the
/// agenthub client dispatches on `type`; deserialization accepts either, with
/// `kind` winning when a sender writes both.
#[derive(Debug, Clone, PartialEq)]
pub enum Part {
    Text(String),
    File(FileContent),
    Data(Value),
}

impl Part {
    /// A text part.
    pub fn text(text: impl Into<String>) -> Self {
        Part::Text(text.into())
    }

    /// The text, when this is a text part.
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Part::Text(text) => Some(text),
            Part::File(_) | Part::Data(_) => None,
        }
    }

    /// The canonical discriminator (`text` / `file` / `data`).
    pub fn kind(&self) -> &'static str {
        match self {
            Part::Text(_) => PART_TEXT,
            Part::File(_) => PART_FILE,
            Part::Data(_) => PART_DATA,
        }
    }
}

impl Serialize for Part {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = Map::new();
        map.insert("kind".into(), Value::String(self.kind().into()));
        // agenthub's own format says `type`; a part without it reads as empty
        // text over there, so both are always written.
        map.insert("type".into(), Value::String(self.kind().into()));
        match self {
            Part::Text(text) => {
                map.insert("text".into(), Value::String(text.clone()));
            }
            Part::File(file) => {
                map.insert("file".into(), file.to_json());
            }
            Part::Data(data) => {
                map.insert("data".into(), data.clone());
            }
        }
        Serialize::serialize(&map, serializer)
    }
}

impl<'de> Deserialize<'de> for Part {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let object = value
            .as_object()
            .ok_or_else(|| de::Error::custom("part must be an object"))?;
        let tag = object
            .get("kind")
            .or_else(|| object.get("type"))
            .and_then(Value::as_str)
            .ok_or_else(|| de::Error::custom("part needs kind or type"))?;
        match tag {
            PART_TEXT => object
                .get("text")
                .and_then(Value::as_str)
                .map(|text| Part::Text(text.to_string()))
                .ok_or_else(|| de::Error::custom("text part needs text")),
            PART_FILE => object
                .get("file")
                .cloned()
                .ok_or_else(|| de::Error::custom("file part needs file"))
                .and_then(|file| {
                    serde_json::from_value(file)
                        .map(Part::File)
                        .map_err(de::Error::custom)
                }),
            PART_DATA => object
                .get("data")
                .cloned()
                .map(Part::Data)
                .ok_or_else(|| de::Error::custom("data part needs data")),
            other => Err(de::Error::custom(format!("unknown part kind: {other}"))),
        }
    }
}

/// One turn between the caller and the agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    #[serde(default = "message_kind")]
    pub kind: String,
    pub role: Role,
    /// The spec requires it; an empty id is tolerated on input (the cloud crate
    /// fills one in) and omitted on output instead of writing `""`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message_id: String,
    pub parts: Vec<Part>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    /// The v0.1 name for `contextId`; still sent by some clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

fn message_kind() -> String {
    "message".to_string()
}

impl Message {
    /// A message with no ids; the caller fills them if it needs them.
    pub fn new(role: Role, parts: Vec<Part>) -> Self {
        Self {
            kind: message_kind(),
            role,
            message_id: String::new(),
            parts,
            context_id: None,
            task_id: None,
            session_id: None,
            metadata: None,
        }
    }

    /// Concatenated text of every text part (the only input this server reads).
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.parts {
            if let Some(text) = part.as_text() {
                out.push_str(text);
            }
        }
        out
    }

    /// `contextId`, falling back to the v0.1 `sessionId`.
    pub fn context(&self) -> Option<&str> {
        self.context_id.as_deref().or(self.session_id.as_deref())
    }
}

/// Lifecycle snapshot of a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatus {
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<String>,
}

impl TaskStatus {
    /// A status in `state`, stamped now.
    pub fn now(state: TaskState) -> Self {
        Self {
            state,
            message: None,
            timestamp: Some(crate::time::now_rfc3339()),
        }
    }

    /// A status in `state` carrying a human-readable note.
    pub fn with_message(state: TaskState, note: impl Into<String>) -> Self {
        Self {
            state,
            message: Some(Message::new(Role::Agent, vec![Part::text(note)])),
            timestamp: Some(crate::time::now_rfc3339()),
        }
    }
}

/// A named, appendable output produced while running.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub parts: Vec<Part>,
    #[serde(default)]
    pub index: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl Artifact {
    /// A single-text-part artifact at `index`.
    pub fn text(name: impl Into<String>, index: u32, text: impl Into<String>) -> Self {
        Self {
            artifact_id: String::new(),
            name: Some(name.into()),
            description: None,
            parts: vec![Part::text(text)],
            index,
            append: None,
            last_chunk: None,
            metadata: None,
        }
    }
}

/// The unit of work exchanged over A2A.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    #[serde(default = "task_kind")]
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub status: TaskStatus,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history: Option<Vec<Message>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

fn task_kind() -> String {
    "task".to_string()
}

impl Task {
    /// A `submitted` task for `message`.
    pub fn submitted(
        id: impl Into<String>,
        context_id: impl Into<String>,
        message: Message,
    ) -> Self {
        Self {
            kind: task_kind(),
            id: id.into(),
            context_id: context_id.into(),
            session_id: None,
            status: TaskStatus::now(TaskState::Submitted),
            artifacts: Vec::new(),
            history: Some(vec![message]),
            metadata: None,
        }
    }
}

/// A `status-update` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskStatusUpdateEvent {
    #[serde(default = "status_update_kind")]
    pub kind: String,
    /// Legacy dispatch name (`task_status_update`); clients that read `kind`
    /// ignore it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_id: String,
    pub status: TaskStatus,
    #[serde(rename = "final")]
    pub is_final: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

fn status_update_kind() -> String {
    "status-update".to_string()
}

impl TaskStatusUpdateEvent {
    /// A non-final progress update.
    pub fn progress(
        task_id: impl Into<String>,
        context_id: impl Into<String>,
        status: TaskStatus,
    ) -> Self {
        Self {
            kind: status_update_kind(),
            event: Some(event_name("status-update").to_string()),
            task_id: task_id.into(),
            context_id: context_id.into(),
            status,
            is_final: false,
            metadata: None,
        }
    }

    /// The frame that ends a run (or parks it on `input-required`).
    pub fn terminal(
        task_id: impl Into<String>,
        context_id: impl Into<String>,
        status: TaskStatus,
    ) -> Self {
        Self {
            kind: status_update_kind(),
            event: Some(event_name("status-update").to_string()),
            task_id: task_id.into(),
            context_id: context_id.into(),
            status,
            is_final: true,
            metadata: None,
        }
    }
}

/// An `artifact-update` frame.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskArtifactUpdateEvent {
    #[serde(default = "artifact_update_kind")]
    pub kind: String,
    /// Legacy dispatch name (`task_artifact_update`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
    pub task_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub context_id: String,
    pub artifact: Artifact,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_chunk: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

fn artifact_update_kind() -> String {
    "artifact-update".to_string()
}

impl TaskArtifactUpdateEvent {
    /// A streaming chunk for `artifact`; `append` decides whether the client
    /// concatenates or replaces.
    pub fn chunk(
        task_id: impl Into<String>,
        context_id: impl Into<String>,
        artifact: Artifact,
        append: bool,
        last_chunk: bool,
    ) -> Self {
        Self {
            kind: artifact_update_kind(),
            event: Some(event_name("artifact-update").to_string()),
            task_id: task_id.into(),
            context_id: context_id.into(),
            artifact,
            append: Some(append),
            last_chunk: Some(last_chunk),
            metadata: None,
        }
    }
}

/// One envelope on `message/stream` or `/a2a/stream`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamFrame {
    Task(Task),
    StatusUpdate(TaskStatusUpdateEvent),
    ArtifactUpdate(TaskArtifactUpdateEvent),
    Message(Message),
}

impl StreamFrame {
    /// The canonical `kind` discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            StreamFrame::Task(_) => "task",
            StreamFrame::StatusUpdate(_) => "status-update",
            StreamFrame::ArtifactUpdate(_) => "artifact-update",
            StreamFrame::Message(_) => "message",
        }
    }

    /// The SSE `event:` name for this frame.
    pub fn event(&self) -> &'static str {
        event_name(self.kind())
    }

    /// Whether a reader may stop after this frame.
    pub fn is_final(&self) -> bool {
        match self {
            StreamFrame::StatusUpdate(update) => update.is_final,
            StreamFrame::Task(task) => task.status.state.is_terminal(),
            StreamFrame::ArtifactUpdate(_) | StreamFrame::Message(_) => false,
        }
    }
}

/// `message/send` and `message/stream` payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSendParams {
    pub message: Message,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub configuration: Option<MessageSendConfiguration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

/// Per-call tuning.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MessageSendConfiguration {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub accepted_output_modes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    /// Accepted for compatibility; `message/send` always blocks to a settled
    /// state, so `false` changes nothing today.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notification: Option<PushNotificationConfig>,
}

/// Where to POST task state changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushNotificationConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authentication: Option<Authentication>,
}

/// The credential schemes a push receiver expects.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Authentication {
    #[serde(default)]
    pub schemes: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credentials: Option<String>,
}

/// `tasks/get` and `tasks/cancel` payload (`id`, with `taskId` as alias).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskQueryParams {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub history_length: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl TaskQueryParams {
    /// The addressed task: `id` wins, `taskId` is the fallback.
    pub fn resolve_id(&self) -> &str {
        if self.id.is_empty() {
            &self.task_id
        } else {
            &self.id
        }
    }
}

/// `tasks/resume` payload. `message` is optional because a caller may answer an
/// approval through `metadata` only.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskResumeParams {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub task_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<Value>,
}

impl TaskResumeParams {
    /// The addressed task: `id` wins, `taskId` is the fallback.
    pub fn resolve_id(&self) -> &str {
        if self.id.is_empty() {
            &self.task_id
        } else {
            &self.id
        }
    }
}

/// `tasks/pushNotification/set` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushSetParams {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notification_config: Option<PushNotificationConfig>,
    /// v0.1 spelling of the same object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub push_notification: Option<PushNotificationConfig>,
}

impl PushSetParams {
    /// The addressed task: `taskId` wins, `id` is the fallback.
    pub fn resolve_id(&self) -> &str {
        if self.task_id.is_empty() {
            &self.id
        } else {
            &self.task_id
        }
    }

    /// The config, under either spelling.
    pub fn config(&self) -> Option<&PushNotificationConfig> {
        self.push_notification_config
            .as_ref()
            .or(self.push_notification.as_ref())
    }
}

/// `tasks/pushNotification/get` payload.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushGetParams {
    #[serde(default)]
    pub task_id: String,
    #[serde(default)]
    pub id: String,
}

impl PushGetParams {
    /// The addressed task: `taskId` wins, `id` is the fallback.
    pub fn resolve_id(&self) -> &str {
        if self.task_id.is_empty() {
            &self.id
        } else {
            &self.task_id
        }
    }
}
