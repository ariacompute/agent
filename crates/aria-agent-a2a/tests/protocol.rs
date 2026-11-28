//! Wire-contract tests: whatever a client reads off the socket is pinned here.
//!
//! Two invariants are load-bearing and easy to break silently, so they get
//! explicit tests:
//!
//! * a part is written with **both** `kind` and `type` (agenthub dispatches on
//!   `type`, the specification on `kind`);
//! * a terminal frame carries `final: true` and there is no sentinel frame.

use aria_agent_a2a::card::{AgentSkill, Capabilities};
use aria_agent_a2a::jsonrpc::{code, code_name, Method, Request, Response, RpcError};
use aria_agent_a2a::state::TaskState;
use aria_agent_a2a::stream::{encode, event_name, sse_frame};
use aria_agent_a2a::time::{format_rfc3339, now_rfc3339};
use aria_agent_a2a::types::{
    Artifact, FileContent, Message, MessageSendConfiguration, MessageSendParams, Part,
    PushGetParams, PushNotificationConfig, PushSetParams, Role, StreamFrame, Task,
    TaskArtifactUpdateEvent, TaskQueryParams, TaskResumeParams, TaskStatus, TaskStatusUpdateEvent,
};
use aria_agent_a2a::{AgentCard, Authentication, Provider};
use serde_json::{json, Value};

fn text_message(text: &str) -> Message {
    Message::new(Role::User, vec![Part::text(text)])
}

// ---------------------------------------------------------------- card

#[test]
fn card_for_agent_advertises_the_standard_surface() {
    let card = AgentCard::for_agent("demo", "Demo agent", "http://host/a2a", "0.1.0");
    assert_eq!(card.protocol_version, "1.0.0");
    assert_eq!(card.name, "demo");
    assert_eq!(card.description, "Demo agent");
    assert_eq!(card.url, "http://host/a2a");
    assert_eq!(card.version, "0.1.0");
    assert!(card.capabilities.streaming);
    assert!(card.capabilities.push_notifications);
    assert!(card.capabilities.state_transition_history);
    assert_eq!(card.default_input_modes, vec!["text".to_string()]);
    assert_eq!(
        card.default_output_modes,
        vec!["text".to_string(), "data".to_string()]
    );
    assert_eq!(card.skills.len(), 1);
    assert_eq!(card.skills[0].id, "chat");
    assert_eq!(card.skills[0].name, "demo");
    assert_eq!(card.authentication.as_ref().unwrap().schemes, ["bearer"]);
    assert_eq!(card.provider.as_ref().unwrap().organization, "ariacompute");
    assert!(card.documentation_url.is_none());
    assert!(card.metadata.is_none());

    let wire = serde_json::to_value(&card).expect("card serializes");
    assert_eq!(wire["protocolVersion"], "1.0.0");
    assert_eq!(wire["capabilities"]["pushNotifications"], true);
    // No secrets, no instructions, no tool families.
    assert!(wire.get("instructions").is_none());
    assert!(wire.get("tools").is_none());
}

#[test]
fn card_deserialization_defaults_the_protocol_version_and_capabilities() {
    let card: AgentCard = serde_json::from_value(json!({
        "name": "n", "description": "d", "url": "u", "version": "1"
    }))
    .expect("minimal card");
    assert_eq!(card.protocol_version, "1.0.0");
    assert_eq!(card.capabilities, Capabilities::default());
    assert!(card.skills.is_empty());
    assert!(card.authentication.is_none());
    assert!(card.provider.is_none());
}

#[test]
fn card_survives_a_round_trip_with_skills_and_provider() {
    let card = AgentCard {
        protocol_version: "1.0.0".into(),
        name: "n".into(),
        description: "d".into(),
        url: "u".into(),
        version: "1".into(),
        capabilities: Capabilities {
            streaming: true,
            push_notifications: false,
            state_transition_history: true,
        },
        default_input_modes: vec!["text".into()],
        default_output_modes: vec!["text".into()],
        skills: vec![AgentSkill {
            id: "s".into(),
            name: "s".into(),
            description: "d".into(),
            tags: vec!["t".into()],
            examples: vec!["e".into()],
            input_modes: vec!["text".into()],
            output_modes: vec!["text".into()],
            resource_kind: Some("agent".into()),
        }],
        authentication: Some(Authentication {
            schemes: vec!["bearer".into()],
            credentials: Some("token".into()),
        }),
        provider: Some(Provider {
            organization: "org".into(),
            url: Some("https://example.test".into()),
        }),
        documentation_url: Some("https://docs.test".into()),
        metadata: Some(json!({"k": "v"})),
    };
    let wire = serde_json::to_string(&card).expect("card serializes");
    let back: AgentCard = serde_json::from_str(&wire).expect("card parses");
    assert_eq!(card, back);

    let wire = serde_json::to_value(&card).expect("card serializes");
    assert_eq!(wire["skills"][0]["resourceKind"], "agent");
    assert_eq!(wire["documentationUrl"], "https://docs.test");
}

#[test]
fn endpoint_joins_urls_without_double_slashes() {
    assert_eq!(
        AgentCard::endpoint("http://hub:3000", "/a2a"),
        "http://hub:3000/a2a"
    );
    assert_eq!(
        AgentCard::endpoint("http://hub:3000/", "a2a"),
        "http://hub:3000/a2a"
    );
    assert_eq!(
        AgentCard::endpoint("http://hub:3000/", "/v1/agents/a/a2a"),
        "http://hub:3000/v1/agents/a/a2a"
    );
    assert_eq!(
        AgentCard::endpoint("http://hub:3000/", "/"),
        "http://hub:3000"
    );
    assert_eq!(
        AgentCard::endpoint("http://hub:3000/", ""),
        "http://hub:3000"
    );
}

// ---------------------------------------------------------------- parts

#[test]
fn part_kind_and_text_accessors_cover_every_variant() {
    let text = Part::text("hi");
    assert_eq!(text.kind(), "text");
    assert_eq!(text.as_text(), Some("hi"));
    assert_eq!(Part::File(FileContent::default()).kind(), "file");
    assert_eq!(Part::Data(json!({})).kind(), "data");
    assert_eq!(Part::File(FileContent::default()).as_text(), None);
    assert_eq!(Part::Data(json!({})).as_text(), None);
}

#[test]
fn text_part_writes_both_discriminators() {
    let value = serde_json::to_value(Part::text("hello")).expect("part serializes");
    assert_eq!(value["kind"], "text");
    assert_eq!(value["type"], "text");
    assert_eq!(value["text"], "hello");
}

#[test]
fn text_part_accepts_kind_type_or_both() {
    let by_kind: Part = serde_json::from_value(json!({"kind": "text", "text": "a"})).unwrap();
    let by_type: Part = serde_json::from_value(json!({"type": "text", "text": "b"})).unwrap();
    let by_both: Part =
        serde_json::from_value(json!({"kind": "text", "type": "data", "text": "c"})).unwrap();
    assert_eq!(by_kind, Part::Text("a".into()));
    assert_eq!(by_type, Part::Text("b".into()));
    // `kind` wins when a sender writes both.
    assert_eq!(by_both, Part::Text("c".into()));
}

#[test]
fn file_part_round_trips_every_field_and_the_empty_form() {
    let full = Part::File(FileContent {
        name: Some("a.txt".into()),
        mime_type: Some("text/plain".into()),
        bytes: Some("aGk=".into()),
        uri: Some("file:///a.txt".into()),
        metadata: Some(json!({"k": 1})),
    });
    let value = serde_json::to_value(&full).expect("part serializes");
    assert_eq!(value["kind"], "file");
    assert_eq!(value["file"]["name"], "a.txt");
    assert_eq!(value["file"]["mimeType"], "text/plain");
    assert_eq!(value["file"]["bytes"], "aGk=");
    assert_eq!(value["file"]["uri"], "file:///a.txt");
    assert_eq!(value["file"]["metadata"]["k"], 1);
    assert_eq!(serde_json::from_value::<Part>(value).unwrap(), full);

    let empty = Part::File(FileContent::default());
    let value = serde_json::to_value(&empty).expect("part serializes");
    assert_eq!(value["file"], json!({}));
    assert_eq!(serde_json::from_value::<Part>(value).unwrap(), empty);
}

#[test]
fn data_part_round_trips() {
    let part = Part::Data(json!({"answer": 42}));
    let value = serde_json::to_value(&part).expect("part serializes");
    assert_eq!(value["kind"], "data");
    assert_eq!(value["data"]["answer"], 42);
    assert_eq!(serde_json::from_value::<Part>(value).unwrap(), part);
}

#[test]
fn malformed_parts_are_rejected_with_a_reason() {
    let cases = [
        (json!("text"), "must be an object"),
        (json!({"text": "no tag"}), "needs kind or type"),
        (json!({"type": "text"}), "text part needs text"),
        (json!({"type": "file"}), "file part needs file"),
        (json!({"type": "file", "file": null}), "invalid type: null"),
        (json!({"type": "data"}), "data part needs data"),
        (json!({"type": "video"}), "unknown part kind"),
    ];
    for (value, needle) in cases {
        let error = serde_json::from_value::<Part>(value.clone())
            .expect_err(&format!("{value} must be rejected"));
        assert!(
            error.to_string().contains(needle),
            "{value}: {error} does not mention {needle}"
        );
    }
}

// ---------------------------------------------------------------- messages

#[test]
fn message_text_concatenates_only_text_parts() {
    let message = Message::new(
        Role::User,
        vec![
            Part::text("he"),
            Part::File(FileContent::default()),
            Part::text("llo"),
            Part::Data(json!({"ignored": true})),
        ],
    );
    assert_eq!(message.text(), "hello");
    assert_eq!(message.kind, "message");
    assert!(message.message_id.is_empty());
}

#[test]
fn message_context_falls_back_to_the_v01_session_id() {
    let mut message = text_message("hi");
    assert_eq!(message.context(), None);
    message.session_id = Some("sess-1".into());
    assert_eq!(message.context(), Some("sess-1"));
    message.context_id = Some("ctx-1".into());
    assert_eq!(message.context(), Some("ctx-1"));
}

#[test]
fn message_serialization_omits_empty_ids_and_keeps_aliases() {
    let message = text_message("hi");
    let value = serde_json::to_value(&message).expect("message serializes");
    assert_eq!(value["kind"], "message");
    assert_eq!(value["role"], "user");
    assert!(value.get("messageId").is_none());
    assert!(value.get("contextId").is_none());

    let with_ids: Message = serde_json::from_value(json!({
        "role": "agent",
        "parts": [],
        "messageId": "m1",
        "contextId": "c1",
        "taskId": "t1",
        "sessionId": "s1",
        "metadata": {"k": "v"}
    }))
    .unwrap();
    assert_eq!(with_ids.role, Role::Agent);
    assert_eq!(with_ids.message_id, "m1");
    assert_eq!(with_ids.task_id.as_deref(), Some("t1"));
    assert_eq!(with_ids.session_id.as_deref(), Some("s1"));
    assert_eq!(with_ids.metadata, Some(json!({"k": "v"})));

    let unknown_role: Message =
        serde_json::from_value(json!({"role": "system", "parts": []})).unwrap();
    assert_eq!(unknown_role.role, Role::Other("system".into()));
    assert_eq!(unknown_role.role.as_str(), "system");
    assert_eq!(
        serde_json::to_value(&unknown_role).unwrap()["role"],
        "system"
    );
}

// ---------------------------------------------------------------- tasks

#[test]
fn task_submitted_has_a_history_and_the_task_kind() {
    let task = Task::submitted("t1", "c1", text_message("hi"));
    assert_eq!(task.id, "t1");
    assert_eq!(task.context_id, "c1");
    assert_eq!(task.kind, "task");
    assert_eq!(task.status.state, TaskState::Submitted);
    assert!(task.status.timestamp.is_some());
    assert_eq!(task.history.as_ref().map(Vec::len), Some(1));
    let value = serde_json::to_value(&task).expect("task serializes");
    assert_eq!(value["kind"], "task");
    assert_eq!(value["status"]["state"], "submitted");
    assert!(value.get("artifacts").is_none());
}

#[test]
fn task_and_status_deserialize_with_defaults() {
    let task: Task = serde_json::from_value(json!({
        "id": "t1",
        "status": {"state": "working"}
    }))
    .unwrap();
    assert_eq!(task.kind, "task");
    assert!(task.context_id.is_empty());
    assert!(task.artifacts.is_empty());
    assert!(task.history.is_none());

    let status = TaskStatus::with_message(TaskState::Failed, "boom");
    assert_eq!(status.message.as_ref().unwrap().text(), "boom");
    assert!(status.timestamp.is_some());
}

#[test]
fn artifact_text_builder_has_one_part() {
    let artifact = Artifact::text("final", 2, "answer");
    assert_eq!(artifact.name.as_deref(), Some("final"));
    assert_eq!(artifact.index, 2);
    assert_eq!(artifact.parts.len(), 1);
    assert_eq!(artifact.parts[0].as_text(), Some("answer"));
    assert!(artifact.artifact_id.is_empty());

    let empty = Artifact::default();
    let value = serde_json::to_value(&empty).unwrap();
    assert_eq!(value["parts"], json!([]));
    assert_eq!(value["index"], 0);
}

// ---------------------------------------------------------------- stream frames

#[test]
fn status_update_frames_carry_kind_event_and_final() {
    let progress = TaskStatusUpdateEvent::progress("t1", "c1", TaskStatus::now(TaskState::Working));
    assert!(!progress.is_final);
    assert_eq!(progress.event.as_deref(), Some("task_status_update"));
    let value = serde_json::to_value(&progress).unwrap();
    assert_eq!(value["kind"], "status-update");
    assert_eq!(value["event"], "task_status_update");
    assert_eq!(value["taskId"], "t1");

    let terminal = TaskStatusUpdateEvent::terminal(
        "t1",
        "c1",
        TaskStatus::with_message(TaskState::Completed, "done"),
    );
    assert!(terminal.is_final);
    let value = serde_json::to_value(&terminal).unwrap();
    assert_eq!(value["final"], true);
    assert_eq!(value["status"]["state"], "completed");
}

#[test]
fn status_and_artifact_frames_default_their_kind() {
    let status: TaskStatusUpdateEvent = serde_json::from_value(json!({
        "taskId": "t1", "status": {"state": "working"}, "final": false
    }))
    .unwrap();
    assert_eq!(status.kind, "status-update");
    assert!(status.event.is_none());
    assert!(status.context_id.is_empty());

    let artifact: TaskArtifactUpdateEvent = serde_json::from_value(json!({
        "taskId": "t1", "artifact": {"parts": []}
    }))
    .unwrap();
    assert_eq!(artifact.kind, "artifact-update");
    assert!(artifact.append.is_none());
}

#[test]
fn artifact_update_chunk_marks_append_and_last_chunk() {
    let chunk =
        TaskArtifactUpdateEvent::chunk("t1", "c1", Artifact::text("final", 0, "hi"), true, false);
    assert_eq!(chunk.event.as_deref(), Some("task_artifact_update"));
    assert_eq!(chunk.append, Some(true));
    assert_eq!(chunk.last_chunk, Some(false));
    let value = serde_json::to_value(&chunk).unwrap();
    assert_eq!(value["kind"], "artifact-update");
    assert_eq!(value["artifact"]["parts"][0]["text"], "hi");
}

#[test]
fn stream_frame_classifies_and_reports_finality() {
    let task = StreamFrame::Task(Task::submitted("t1", "c1", text_message("hi")));
    let update = StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
        "t1",
        "c1",
        TaskStatus::now(TaskState::Working),
    ));
    let artifact = StreamFrame::ArtifactUpdate(TaskArtifactUpdateEvent::chunk(
        "t1",
        "c1",
        Artifact::text("final", 0, "hi"),
        true,
        false,
    ));
    let message = StreamFrame::Message(text_message("hi"));

    let kinds = [&task, &update, &artifact, &message];
    let names = ["task", "status-update", "artifact-update", "message"];
    let events = [
        "task",
        "task_status_update",
        "task_artifact_update",
        "message",
    ];
    for ((frame, kind), event) in kinds.iter().zip(names).zip(events) {
        assert_eq!(frame.kind(), kind);
        assert_eq!(frame.event(), event);
    }
    assert!(!task.is_final());
    assert!(!update.is_final());
    assert!(!artifact.is_final());
    assert!(!message.is_final());

    let mut done = Task::submitted("t1", "c1", text_message("hi"));
    done.status = TaskStatus::now(TaskState::Completed);
    assert!(StreamFrame::Task(done).is_final());
    assert!(StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
        "t1",
        "c1",
        TaskStatus::now(TaskState::Canceled)
    ))
    .is_final());
}

#[test]
fn stream_frames_round_trip_through_untagged_serde() {
    let frames = vec![
        StreamFrame::Task(Task::submitted("t1", "c1", text_message("hi"))),
        StreamFrame::StatusUpdate(TaskStatusUpdateEvent::progress(
            "t1",
            "c1",
            TaskStatus::now(TaskState::Working),
        )),
        StreamFrame::ArtifactUpdate(TaskArtifactUpdateEvent::chunk(
            "t1",
            "c1",
            Artifact::text("final", 0, "hi"),
            true,
            true,
        )),
        StreamFrame::Message(Message::new(Role::Agent, vec![Part::text("hi")])),
    ];
    for frame in frames {
        let wire = serde_json::to_string(&frame).expect("frame serializes");
        let back: StreamFrame = serde_json::from_str(&wire).expect("frame parses");
        assert_eq!(frame, back);
    }
}

// ---------------------------------------------------------------- params

#[test]
fn message_send_params_require_a_message() {
    let params: MessageSendParams = serde_json::from_value(json!({
        "message": {"role": "user", "parts": [{"kind": "text", "text": "hi"}]},
        "configuration": {
            "acceptedOutputModes": ["text"],
            "historyLength": 3,
            "blocking": false,
            "pushNotification": {"url": "https://hook.test"}
        }
    }))
    .unwrap();
    assert_eq!(params.message.text(), "hi");
    let config = params.configuration.unwrap();
    assert_eq!(config.accepted_output_modes, ["text"]);
    assert_eq!(config.history_length, Some(3));
    assert_eq!(config.blocking, Some(false));
    assert_eq!(config.push_notification.unwrap().url, "https://hook.test");

    assert!(serde_json::from_value::<MessageSendParams>(json!({})).is_err());
}

#[test]
fn send_configuration_defaults_to_all_absent() {
    let config = MessageSendConfiguration::default();
    let value = serde_json::to_value(&config).unwrap();
    assert_eq!(value, json!({}));
}

#[test]
fn task_query_prefers_id_and_falls_back_to_task_id() {
    let by_id: TaskQueryParams = serde_json::from_value(json!({"id": "a", "taskId": "b"})).unwrap();
    assert_eq!(by_id.resolve_id(), "a");
    let by_alias: TaskQueryParams = serde_json::from_value(json!({"taskId": "b"})).unwrap();
    assert_eq!(by_alias.resolve_id(), "b");
    assert_eq!(TaskQueryParams::default().resolve_id(), "");
}

#[test]
fn resume_params_prefer_id_and_make_the_message_optional() {
    let by_id: TaskResumeParams = serde_json::from_value(
        json!({"id": "a", "taskId": "b", "message": {"role": "user", "parts": []}}),
    )
    .unwrap();
    assert_eq!(by_id.resolve_id(), "a");
    assert!(by_id.message.is_some());
    let by_alias: TaskResumeParams = serde_json::from_value(json!({"taskId": "b"})).unwrap();
    assert_eq!(by_alias.resolve_id(), "b");
    assert!(by_alias.message.is_none());
    assert!(by_alias.metadata.is_none());
}

#[test]
fn push_set_accepts_both_config_spellings() {
    let canonical: PushSetParams = serde_json::from_value(json!({
        "taskId": "t1",
        "pushNotificationConfig": {"url": "https://a.test"}
    }))
    .unwrap();
    assert_eq!(canonical.resolve_id(), "t1");
    assert_eq!(canonical.config().unwrap().url, "https://a.test");

    let alias: PushSetParams = serde_json::from_value(json!({
        "id": "t2",
        "pushNotification": {"url": "https://b.test", "token": "tok"}
    }))
    .unwrap();
    assert_eq!(alias.resolve_id(), "t2");
    let config = alias.config().unwrap();
    assert_eq!(config.url, "https://b.test");
    assert_eq!(config.token.as_deref(), Some("tok"));

    let empty = PushSetParams::default();
    assert_eq!(empty.resolve_id(), "");
    assert!(empty.config().is_none());
}

#[test]
fn push_get_prefers_task_id_and_falls_back_to_id() {
    let canonical: PushGetParams =
        serde_json::from_value(json!({"taskId": "t1", "id": "t2"})).unwrap();
    assert_eq!(canonical.resolve_id(), "t1");
    let alias: PushGetParams = serde_json::from_value(json!({"id": "t2"})).unwrap();
    assert_eq!(alias.resolve_id(), "t2");
    assert_eq!(PushGetParams::default().resolve_id(), "");
}

#[test]
fn push_config_round_trips_authentication() {
    let config = PushNotificationConfig {
        id: Some("p1".into()),
        url: "https://hook.test".into(),
        token: Some("secret".into()),
        authentication: Some(Authentication {
            schemes: vec!["bearer".into()],
            credentials: Some("c".into()),
        }),
    };
    let wire = serde_json::to_string(&config).unwrap();
    let back: PushNotificationConfig = serde_json::from_str(&wire).unwrap();
    assert_eq!(config, back);
    assert_eq!(
        serde_json::to_value(Authentication::default()).unwrap(),
        json!({"schemes": []})
    );
}

// ---------------------------------------------------------------- state

#[test]
fn state_spellings_round_trip() {
    let states = [
        (TaskState::Submitted, "submitted", "submitted"),
        (TaskState::Working, "working", "working"),
        (TaskState::InputRequired, "input-required", "input_required"),
        (TaskState::Completed, "completed", "completed"),
        (TaskState::Failed, "failed", "failed"),
        (TaskState::Canceled, "canceled", "cancelled"),
        (TaskState::Unknown, "unknown", "unknown"),
    ];
    for (state, canonical, alias) in states {
        assert_eq!(state.as_str(), canonical);
        assert_eq!(state.to_string(), canonical);
        assert_eq!(TaskState::parse(canonical), Some(state));
        assert_eq!(TaskState::parse(alias), Some(state));
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            Value::String(canonical.into())
        );
        assert_eq!(
            serde_json::from_value::<TaskState>(json!(alias)).unwrap(),
            state
        );
    }
    assert_eq!(TaskState::parse("  working  "), Some(TaskState::Working));
    assert_eq!(TaskState::parse("paused"), None);
    assert!(serde_json::from_value::<TaskState>(json!("paused")).is_err());
}

#[test]
fn only_the_three_outcomes_are_terminal() {
    assert!(!TaskState::Submitted.is_terminal());
    assert!(!TaskState::Working.is_terminal());
    assert!(!TaskState::InputRequired.is_terminal());
    assert!(TaskState::Completed.is_terminal());
    assert!(TaskState::Failed.is_terminal());
    assert!(TaskState::Canceled.is_terminal());
    assert!(!TaskState::Unknown.is_terminal());
}

#[test]
fn transition_table_admits_progress_and_refresh_only() {
    use TaskState::*;
    // Refreshing a live state is allowed; refreshing a terminal one is not.
    for live in [Submitted, Working, InputRequired, Unknown] {
        assert_eq!(live.can_transition(live), !live.is_terminal());
    }
    for done in [Completed, Failed, Canceled] {
        assert!(!done.can_transition(done));
        for next in [
            Submitted,
            Working,
            InputRequired,
            Completed,
            Failed,
            Canceled,
            Unknown,
        ] {
            assert!(
                !done.can_transition(next),
                "{done} -> {next} must be refused"
            );
        }
    }

    for next in [Working, Failed, Canceled] {
        assert!(Submitted.can_transition(next));
    }
    assert!(!Submitted.can_transition(Completed));
    assert!(!Submitted.can_transition(InputRequired));
    assert!(!Submitted.can_transition(Unknown));

    for next in [InputRequired, Completed, Failed, Canceled] {
        assert!(Working.can_transition(next));
    }
    assert!(!Working.can_transition(Submitted));
    assert!(!Working.can_transition(Unknown));

    for next in [Working, Completed, Failed, Canceled] {
        assert!(InputRequired.can_transition(next));
    }
    assert!(!InputRequired.can_transition(Submitted));
    assert!(!InputRequired.can_transition(Unknown));

    assert!(!Unknown.can_transition(Working));
}

// ---------------------------------------------------------------- jsonrpc

#[test]
fn error_codes_have_stable_names() {
    let codes = [
        (code::PARSE_ERROR, "parse_error"),
        (code::INVALID_REQUEST, "invalid_request"),
        (code::METHOD_NOT_FOUND, "method_not_found"),
        (code::INVALID_PARAMS, "invalid_params"),
        (code::INTERNAL_ERROR, "internal_error"),
        (code::TASK_NOT_FOUND, "task_not_found"),
        (code::TASK_NOT_CANCELABLE, "task_not_cancelable"),
        (code::PUSH_NOT_SUPPORTED, "push_not_supported"),
        (code::AGENT_NOT_FOUND, "agent_not_found"),
        (code::INVALID_STATE_TRANSITION, "invalid_state_transition"),
        (code::RESOURCE_NOT_FOUND, "resource_not_found"),
        (code::TASK_NOT_RESUMABLE, "task_not_resumable"),
        (code::UNSUPPORTED_OPERATION, "unsupported_operation"),
    ];
    for (code, name) in codes {
        assert_eq!(code_name(code), Some(name));
    }
    assert_eq!(code_name(1), None);
}

#[test]
fn request_defaults_jsonrpc_and_params() {
    let request: Request = serde_json::from_value(json!({"method": "tasks/get"})).unwrap();
    assert_eq!(request.jsonrpc, "2.0");
    assert!(request.id.is_none());
    assert_eq!(request.params, Value::Null);

    let full: Request = serde_json::from_value(
        json!({"jsonrpc": "2.0", "id": 7, "method": "tasks/get", "params": {"id": "t"}}),
    )
    .unwrap();
    assert_eq!(full.id, Some(json!(7)));
    assert_eq!(full.params["id"], "t");
    let value = serde_json::to_value(&full).unwrap();
    assert_eq!(value["id"], 7);
    assert_eq!(value["method"], "tasks/get");
}

#[test]
fn responses_carry_result_or_error_but_never_both() {
    let ok = Response::ok(Some(json!(1)), json!({"id": "t1"}));
    assert!(ok.error.is_none());
    assert_eq!(ok.jsonrpc, "2.0");
    let value = serde_json::to_value(&ok).unwrap();
    assert!(value.get("error").is_none());
    assert_eq!(value["result"]["id"], "t1");

    let failed = Response::failed(Some(json!(2)), RpcError::new(code::TASK_NOT_FOUND, "gone"));
    assert!(failed.result.is_none());
    let value = serde_json::to_value(&failed).unwrap();
    assert!(value.get("result").is_none());
    assert_eq!(value["error"]["code"], -32001);
    assert_eq!(value["error"]["data"]["code"], "task_not_found");

    let back: Response = serde_json::from_value(value).unwrap();
    assert_eq!(back.id, Some(json!(2)));
    assert!(back.error.is_some());
}

#[test]
fn rpc_error_omits_data_for_an_unknown_code() {
    let unknown = RpcError::new(1234, "boom");
    assert!(unknown.data.is_none());
    assert_eq!(serde_json::to_value(&unknown).unwrap()["message"], "boom");

    let with_data = RpcError::with_data(code::INVALID_PARAMS, "bad", json!({"field": "message"}));
    assert_eq!(with_data.data, Some(json!({"field": "message"})));
}

#[test]
fn method_names_and_legacy_aliases_resolve() {
    let methods = [
        ("message/send", Method::MessageSend),
        ("tasks/send", Method::MessageSend),
        ("message/stream", Method::MessageStream),
        ("tasks/sendSubscribe", Method::MessageStream),
        ("tasks/get", Method::TasksGet),
        ("tasks/cancel", Method::TasksCancel),
        ("tasks/resume", Method::TasksResume),
        ("tasks/pushNotification/set", Method::PushSet),
        ("tasks/pushNotification/get", Method::PushGet),
    ];
    for (name, method) in methods {
        assert_eq!(Method::parse(name), Some(method));
        assert_eq!(Method::parse(&format!("  {name}  ")), Some(method));
    }
    assert_eq!(Method::parse("tasks/pause"), None);
    assert_eq!(Method::parse(""), None);

    assert_eq!(Method::MessageSend.name(), "message/send");
    assert_eq!(Method::MessageStream.name(), "message/stream");
    assert_eq!(Method::TasksGet.name(), "tasks/get");
    assert_eq!(Method::TasksCancel.name(), "tasks/cancel");
    assert_eq!(Method::TasksResume.name(), "tasks/resume");
    assert_eq!(Method::PushSet.name(), "tasks/pushNotification/set");
    assert_eq!(Method::PushGet.name(), "tasks/pushNotification/get");

    assert!(Method::MessageSend.needs_write());
    assert!(Method::MessageStream.needs_write());
    assert!(Method::TasksCancel.needs_write());
    assert!(Method::TasksResume.needs_write());
    assert!(Method::PushSet.needs_write());
    assert!(!Method::TasksGet.needs_write());
    assert!(!Method::PushGet.needs_write());
}

// ---------------------------------------------------------------- sse

#[test]
fn event_names_map_kind_to_the_legacy_name() {
    assert_eq!(event_name("status-update"), "task_status_update");
    assert_eq!(event_name("artifact-update"), "task_artifact_update");
    assert_eq!(event_name("message"), "message");
    assert_eq!(event_name("task"), "task");
    assert_eq!(event_name("something-else"), "task");
}

#[test]
fn sse_block_is_one_event_line_and_one_data_line() {
    assert_eq!(
        sse_frame("message", "{\"a\":1}"),
        "event: message\ndata: {\"a\":1}\n\n"
    );
}

#[test]
fn encoded_frames_are_valid_sse_and_reparse() {
    let frames = [
        StreamFrame::Task(Task::submitted("t1", "c1", text_message("hi"))),
        StreamFrame::StatusUpdate(TaskStatusUpdateEvent::terminal(
            "t1",
            "c1",
            TaskStatus::now(TaskState::Completed),
        )),
        StreamFrame::ArtifactUpdate(TaskArtifactUpdateEvent::chunk(
            "t1",
            "c1",
            Artifact::text("final", 0, "hi"),
            true,
            false,
        )),
        StreamFrame::Message(Message::new(Role::Agent, vec![Part::text("hi")])),
    ];
    for frame in frames {
        let block = encode(&frame);
        assert!(block.ends_with("\n\n"), "{block:?} has no terminator");
        let mut lines = block.lines();
        let event = lines.next().expect("event line");
        assert_eq!(event, format!("event: {}", frame.event()));
        let data = lines.next().expect("data line");
        let payload = data.strip_prefix("data: ").expect("data prefix");
        let parsed: Value = serde_json::from_str(payload).expect("frame JSON");
        assert_eq!(parsed["kind"], frame.kind());
        assert!(!block.contains("[DONE]"));
    }
}

// ---------------------------------------------------------------- time

#[test]
fn epoch_and_leap_day_format_exactly() {
    assert_eq!(format_rfc3339(0, 0), "1970-01-01T00:00:00.000Z");
    assert_eq!(format_rfc3339(1_709_164_800, 0), "2024-02-29T00:00:00.000Z");
    assert_eq!(
        format_rfc3339(1_709_164_800 + 86_399, 999_999_999),
        "2024-02-29T23:59:59.999Z"
    );
    assert_eq!(format_rfc3339(951_868_800, 0), "2000-03-01T00:00:00.000Z");
}

#[test]
fn year_boundaries_and_pre_epoch_seconds_are_correct() {
    assert_eq!(format_rfc3339(31_535_999, 0), "1970-12-31T23:59:59.000Z");
    assert_eq!(format_rfc3339(-1, 0), "1969-12-31T23:59:59.000Z");
    // A date before the 0000-03-01 epoch shift exercises the negative era.
    assert_eq!(
        format_rfc3339(-62_167_219_200, 0),
        "0000-01-01T00:00:00.000Z"
    );
}

#[test]
fn now_is_rfc3339_with_milliseconds() {
    let now = now_rfc3339();
    assert_eq!(now.len(), 24, "{now}");
    assert!(now.ends_with('Z'), "{now}");
    assert_eq!(&now[4..5], "-");
    assert_eq!(&now[10..11], "T");
}
