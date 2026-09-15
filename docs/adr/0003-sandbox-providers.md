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
  * `DockerSandbox` — **default**, talks to the Docker **Engine API** over
    `bollard` (no `docker` CLI needed).
  * `KataSandbox` — `docker --runtime=kata`.
  * `CubeSandbox` — `cube` CLI (extend flags per deployment).
* Selection is config-driven via `SandboxProvider` / `AgentConfig.sandbox_provider`.
* `DockerSandbox` connects **lazily** and never panics: `new()` only records how
  to reach the daemon; the client is resolved on the first `spawn` / `exec` /
  `destroy`. Resolution order is `DOCKER_HOST` (also `tcp://…` and the Windows
  named pipe) → `ARIA_DOCKER_SOCKET` → well-known sockets
  (`/var/run/docker.sock`, `~/.docker/run/docker.sock` for **Docker Desktop on
  macOS**, Colima, rootless Podman). If none is reachable, the call returns
  `SandboxError::NotConfigured` instead of aborting the process.
* When deeper integration is wanted, `aria-agent-sandbox` can depend on codex's
  `sandboxing` / `linux-sandbox` crates as precise path dependencies.

## Consequences
* Providers assume the corresponding CLI/daemon is installed or reachable on the
  host; a missing daemon is a **runtime error** of the sandbox call, never a
  startup panic (so the SDK, the cloud service and `cargo test` all work on
  macOS / Docker-less machines).
* `/var/run/docker.sock` is Linux-specific: macOS (Docker Desktop), Colima and
  rootless setups need the discovery list or an explicit `DOCKER_HOST` /
  `ARIA_DOCKER_SOCKET`.
* Adding a provider = implement `Sandbox` + register in `from_provider`.
