# ADR-0016 — Tool execution pipeline, permission presets and fail-closed approvals

Status: Accepted.

## Context

Tool calls used to be dispatched directly: resolve the name, run it. Nothing
bounded the file system to a directory, `shell` accepted string interpolation, and
a deployment without an approval channel effectively auto-approved everything.
That is a silent privilege escalation, not a policy.

## Decision

Every tool call goes through five fixed stages, in this order:

1. **Resolve** — find the handler by name; an unknown tool is an error result.
2. **Preset check** — `PermissionPreset::{ReadOnly, WorkspaceWrite,
   DangerFullAccess}` decides whether the tool may run at all. `ReadOnly` allows
   only the read-only families (`fs_read`, `fs_list`, `job_output`, `team_status`,
   `subagent_list`, `schedule_list`, `ask_user`, …) and refuses mutations before
   any approval is consulted.
3. **Approval** — `ApprovalBroker::decide` returns `Allow`, `Deny { reason }` or
   `Unavailable { reason }`. **No broker, or `Unavailable`, means the call is
   refused** and the reason is fed back to the model as the tool result. There is
   no "assume yes" path, and no silent drop.
4. **Dispatch** — `ShellSeam` / `FsSeam` / `JobSeam` either run (argv arrays only,
   never shell strings) or report `Unsupported`/`Unavailable`. A family whose
   provider is missing is not offered at all.
5. **Post-process** — results above `TOOL_RESULT_MAX_BYTES` (default 64 KiB) are
   truncated and spilled to a file; the model receives a locator plus a retrieval
   hint, never the whole payload.

`SandboxPolicy` holds one root directory; `fs` canonicalises every path and
rejects `..`/symlink escapes outside it, and `shell`/`jobs` run with that same root
as their working directory. `web_fetch` refuses loopback/private/link-local hosts,
limits redirects and response size, and only accepts text content types.

Approval is **synchronous**. A real human-in-the-loop would need a bidirectional
SSE channel and UI; until that exists, the honest answer is a refusal with a
reason.

## Consequences

* Three fail-closed cases are unit-tested: no approval provider, no `web`
  provider, no `fs` root.
* Deployments that want unattended mutation must say so explicitly
  (`approval_mode = auto` + a write preset) — a documented trust decision, not a
  default.
* The model always learns *why* a call was refused, so it can adapt instead of
  retrying blindly.
* Tool result truncation keeps context windows predictable.
