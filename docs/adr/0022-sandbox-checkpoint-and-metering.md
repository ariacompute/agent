# ADR-0022 — Sandbox checkpoints and state-based metering

- Status: accepted
- Date: 2026-11-19

## Context

Two failures are common in agent execution layers:

1. **Restore bypasses the quota.** Creation is limited, but restoring from a
   snapshot is a second creation path that someone forgot to guard.
2. **Idle work is not cheaper.** A sandbox that is paused or suspended still
   bills at its running rate, so "pause" saves nothing and nobody uses it.

## Decision

### Checkpoints

`CheckpointRef { id, image, source, created_at_ms }` is produced by committing the
container (filesystem + process state). Semantics:

* **restore always derives a new sandbox** — the source is never rolled back, so
  a checkpoint is repeatable and a failed restore cannot damage its origin;
* backends declare support through `SandboxCapabilities`; unsupported operations
  return `SandboxError::Unsupported`, never a silent success;
* Kata delegates to the Docker implementation; Cube and other backends fall back
  to the fail-closed defaults.

### Lifecycle state is orthogonal to task state

`SandboxLifecycle` = `creating | running | paused | suspended | destroyed | failed`.
Illegal transitions are errors listing the allowed set.

The task state (`queued | in progress | completed | failed`) lives in
`AgentEvent`/`session_turns` and is **never** derived from `SandboxLifecycle`.
`status: running` answers "is the box up", never "did the job finish".

### Metering by state

| state | vCPU | memory | disk |
|---|---|---|---|
| running | ✓ | ✓ | ✓ |
| paused | ✗ | ✓ | ✓ |
| suspended | ✗ | ✗ | ✓ |
| destroyed | ✗ | ✗ | ✗ |

Accounting is integer-only: `vcpu_millis`, `memory_mib_millis`,
`disk_mib_millis` (unit × milliseconds). Pricing is a separate, configurable
`PriceBook` in micro-USD; the ledger never hardcodes a vendor rate.

### One quota gate

`QuotaGuardSandbox` wraps **any** backend and calls `MeteringSeam::reserve` from
both `spawn` and `restore`. A refused reservation destroys the just-created
sandbox, so a denied restore cannot leave a container billing without a
reservation.

### Estimates are not money

`token_meter::estimate_tokens` (`ceil(chars/4)`) remains a context-pressure
signal. `EstimatedTokens` and `BilledTokens` are distinct types: only
provider-reported usage can reach a metering path.

## Consequences

* `spawn` and `restore` provably share a gate — it is the same function call, not
  a convention.
* Pausing has a visible financial effect, which is the only reason an operator
  will pause instead of destroying.
* Backends without snapshot support fail loudly; the API returns `400` with
  "no sandbox backend is configured" or the backend's `Unsupported` message.
