# ADR-0018 — Removing the vendored codex harness

Status: Accepted.

## Context

The runtime started as a wrapper around OpenAI's `codex` harness, vendored as a git
submodule and promoted to the workspace root by a patch. The cost became hard to
justify:

* 15 `rama-*` crates pinned to exact versions in `Cargo.toml`, which made every
  dependency bump a manual exercise and lengthened CI.
* A submodule initialization step plus a patch application before **any** build.
* A Windows-only `winapi` patch to reconcile `c_void` between codex's PTY crate and
  the standard library.
* A `CodexSandbox` provider that duplicated capabilities we already had as seams.

Nothing in the shipped product needed codex crates: the sandbox, memory and loop
had their own implementations.

## Decision

Remove codex entirely:

* delete the `codex/` submodule and `.gitmodules`;
* drop every `git = "…/codex"` dependency, the exact `rama-*` pins and the
  `[patch.crates-io]` section;
* delete the `CodexSandbox` provider and `SandboxProvider::Codex`;
* delete the codex-specific ADRs (0001 submodule strategy, 0005 integration) and
  rewrite the docs that referenced them.

**No alias, no fallback.** `sandbox_provider = "codex"` is an unknown provider:
`SandboxProvider::parse` returns `None` and the runtime fails with a configuration
error. Docker stays the default; Kata, Cube and Browser remain selectable.

## Consequences

* `cargo build` / `cargo test` work on a fresh clone with no submodule step, and
  the dependency graph no longer contains a vendored workspace.
* The PTY capability moved to `aria-agent-tools` behind an off-by-default `pty`
  feature (`portable-pty`), so mobile builds do not compile it; the Windows
  `winapi` feature note moved with it.
* Deployments that still set `sandbox_provider = "codex"` now fail at startup with
  a clear message instead of silently running on a different backend.
* The seams (ADR-0014) are now the only extension mechanism, which is what the
  plugin registry expects.
