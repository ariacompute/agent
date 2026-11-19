# ADR-0023 — Idle detection and credential rotation

- Status: accepted
- Date: 2026-11-19

## Context

Two narrow signals cause two predictable incidents:

* **Idle = no inbound HTTP** suspends a sandbox in the middle of a build, a
  background job or an interactive session.
* **Rotation behind a TTL cache** means a leaked credential keeps working until
  the cache expires — exactly when an operator needs it dead *now*.

## Decision

### Multi-signal idle detection

`IdleSignals` samples five independent sources:

| signal | meaning |
|---|---|
| `last_http_request_ms` | inbound request through the proxy/published port |
| `last_exec_ms` | last command run in the sandbox |
| `running_processes` | live processes inside the sandbox |
| `open_connections` | open sockets |
| `pending_jobs` | jobs queued or running in the jobs seam |

Any signal keeps the sandbox active; the verdict names the strongest one so a
log line explains the decision. Both the timeout and the scan period are
configurable, `idle_timeout_secs = 0` disables suspension, and the monitor never
decides on its own — it returns a `IdleVerdict` the caller acts on.

`quiet_for_ms` is reported with the verdict, so "why was this suspended at 03:00"
has an answer.

### Rotation invalidates immediately

* The store has **no read cache**: every `resolve` decrypts the current version.
  A rotation therefore takes effect on the next request, with no TTL window.
* `rotate` is atomic (write temp file, `rename`) and bumps `version`; the version
  is part of `SecretMeta` so clients can detect a change.
* Secrets are sealed with CHACHA20-POLY1305 under a key derived from
  `AGENT_CREDENTIAL_MASTER_KEY` (SHA-256 of the passphrase). The master key is
  never persisted; a missing key is a hard startup error.
* `rotate_after_secs` records a deadline; `due_for_rotation(now)` reports names
  whose deadline passed so a scheduler can act.

## Consequences

* A sandbox doing work is never suspended because it happens not to speak HTTP.
* Revocation is synchronous with the write; operators do not reason about TTLs.
* Rotation is *detectable* but not self-driving: the deadline is reported, and the
  scheduler that consumes it is a deployment concern (documented, not hidden).
