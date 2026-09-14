# ADR 0003: sandbox providers

## Status
Accepted.

## Context
Tool execution must be isolated and swappable between environments. codex ships
`linux-sandbox` / `bwrap` / `mxc-sandbox` but **not** docker / kata / cubesandbox,
which the platform requires.

## Decision
* Define a single `Sandbox` trait (`spawn` / `exec` / `destroy`) with an
  `ExecSpec` / `ExecOutput` shape mirroring codex's `sandboxing` crate.
* Ship three providers behind one abstraction:
  * `DockerSandbox` — **default**, shells out to `docker`.
  * `KataSandbox` — `docker --runtime=kata`.
  * `CubeSandbox` — `cube` CLI (extend flags per deployment).
* Selection is config-driven via `SandboxProvider` / `AgentConfig.sandbox_provider`.
* When deeper integration is wanted, `aria-agent-sandbox` can depend on codex's
  `sandboxing` / `linux-sandbox` crates as precise path dependencies.

## Consequences
* Providers assume the corresponding CLI is installed on the host.
* Adding a provider = implement `Sandbox` + register in `from_provider`.
