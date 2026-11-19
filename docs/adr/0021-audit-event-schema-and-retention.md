# ADR-0021 — Audit: fixed schema, retention, export

- Status: accepted
- Date: 2026-11-19

## Context

Recording is the easiest pillar to claim and the easiest to leave thin. A
governance product that cannot answer "what actually left, when, under which
policy revision, for which tenant" is not governable — and an audit log that
contains secrets is a new incident surface.

## Decision

### Schema

`agent_core::seams::AuditEvent` is the only audit record type:

| field | meaning |
|---|---|
| `id`, `ts_ms` | stable identity and wall clock |
| `principal_id` | tenant — every read filters on it |
| `session_id`, `turn_id` | optional correlation |
| `kind` | fixed vocabulary (below) |
| `target` | host, tool name, sandbox id or secret name |
| `decision` | `Some(true/false)` for decisions |
| `policy_revision` | revision in force at the decision |
| `metadata` | redacted, schema-stable extras |

Kinds: `egress_decision`, `credential_substituted`, `credential_rotated`,
`policy_changed`, `tool_executed`, `compression_applied`,
`lifecycle_transition`, `quota_decision`, `unenforced_egress`.

Wrong spelling is not accepted on the wire: `serde(rename_all = "snake_case")`
with no aliases.

### Redaction

Both sinks sanitise before writing. Any metadata key containing
`authorization`, `api_key`, `apikey`, `token`, `secret`, `password`,
`credential` or `cookie` (recursively, including arrays) is replaced with
`[redacted]`. Secret **values** never enter an event in the first place: the
credential seam reports stub ids and counts only.

### Storage

* Local: `audit-YYYYMMDD.jsonl`, one object per line, append-only.
* Cloud: `audit_events`, indexed on `(principal_id, ts_ms DESC)`.

Retention: `AGENT_AUDIT_RETENTION_DAYS` (default 30, `0` = keep forever).
Expired files are removed on open and on demand.

### Export

`GET /v1/governance/audit/export` returns `application/x-ndjson`, same schema as
the local file, filtered by the caller's principal. This is the format an
incident responder actually wants: one line per event, no paging protocol.

## Consequences

* Audit cannot be silently disabled: `audit_sink=none` still counts drops in
  `NoAudit` and the startup log says so.
* A corrupt line fails the export instead of being skipped — a partial audit
  trail that looks complete is worse than an error.
* The frozen SSE envelope (`event_envelope.rs`) is untouched: audit is a separate
  sink, not a frame field.
