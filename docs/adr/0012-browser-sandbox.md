# ADR-0012: Browser Sandbox

## Status

Accepted.

## Context

The agent needs to operate a real browser from within its execution sandbox so it
can navigate, scrape, interact with, and solve anti‑bot challenges on web pages.
Six engines were evaluated as references:

- **servo** — Rust browser engine (WIP).
- **obscura** — anti‑bot / stealth layer (Chromium‑based fingerprint evasion).
- **Chromium / Playwright** (+ `playwright-captcha`) — most mature automation backend.
- **gosub-engine** — Rust browser engine (WIP).
- **camoufox** — Firefox‑based anti‑detect browser (Playwright compatible).
- **lightpanda** — lightweight headless browser purpose‑built for AI agents.

We must support **all six** as independently deployable container images and let the
agent **dynamically select / switch** the active engine at runtime. The capability
must not break the frozen `AgentEvent` streaming contract (rule 9) or the FFI
boundary (rule 2), and it must reuse the existing Docker/Kata sandbox container path
and its resource limits.

## Decision

1. **A new crate `aria-agent-browser`** holds the browser capability so the heavy
   engine code is isolated from `agent-core` (which exposes only the abstract
   `ToolHandler`). This matches the existing "one capability per crate" layout.
2. **`BrowserCatalog`** maps each `BrowserKind` (servo, obscura, chromium, gosub,
   camoufox, lightpanda) to a container image, with `ARIACOMPUTE_BROWSER_IMAGE_<KIND>`
   overrides and a `ARIACOMPUTE_BROWSER_ENGINE` default (Chromium). Unrecognized
   values fall back without panicking.
3. **A single `runner.mjs`** is baked into every image. It reads a JSON op on its
   command line (`{"engine","op":{...}}`) and writes a JSON result
   (`{"ok","data","error"}`). This keeps the Rust side engine‑agnostic: it only
   spawns a container and forwards ops. Native engines (Servo/Gosub) return
   `unsupported` for interactive ops instead of panicking.
4. **`BrowserSandbox`** implements both `agent_sandbox::Sandbox` (delegating
   container lifecycle to the inner Docker/Kata sandbox) and `BrowserEngine`
   (navigate / extract / click / fill / screenshot / evaluate / solve_captcha /
   switch_to). It keeps one long‑lived browser container (`sleep infinity`) and
   reuses it across tool calls; ops are executed via `node /runner/runner.mjs`.
5. **`switch_to(kind, image?)`** tears down the current container and brings up the
   requested image; on spawn failure it rolls back to the previous engine so the
   session never drops.
6. **`browser_use { engine?, image? }`** is a tool that calls `switch_to`, enabling
   runtime switching from inside the agentic loop.
7. **`ToolHandler`** (new in `agent-core`) is the pluggable dispatch seam:
   `handles(name)` + `run(call, ctx, session) -> ToolResult`. `Agent::with_handlers`
   accepts a `Vec<Arc<dyn ToolHandler>>`; `run_event_stream` / `exec_tool_call`
   consult handlers **before** falling back to the default shell sandbox
   (`sandbox_exec`). The `AgentEvent` contract is unchanged.
8. **Provider wiring**: `agent-sandbox` gains `SandboxProvider::Browser` (parse /
   as_str), but `from_provider` returns `NotConfigured` — mirroring `Codex` — because
   the `BrowserSandbox` lives in `aria-agent-browser`. The cloud and FFI runtimes
   special‑case `sandbox_provider == "browser"`, construct a `BrowserSandbox`, wrap
   it as a `BrowserToolHandler`, and inject it via `Agent::with_handlers` while the
   agent's *shell* fallback stays on the regular Docker sandbox.
9. **Tools**: `browser_tools()` (browser_navigate / browser_extract / browser_click
   / browser_fill / browser_screenshot / browser_evaluate / browser_solve_captcha /
   browser_use) is appended to both `agent_tools()` (cloud) and `sdk_tools()` (FFI).
   No new FFI exports are introduced, so `just ffi` regeneration is **not** required.

## Consequences

- Six browser engines are selectable, each as its own sandbox container image, with
  runtime switching and graceful degradation for native engines.
- `agent-core` depends only on the abstract `ToolHandler`; browser code is fully
  quarantined in `aria-agent-browser`.
- The frozen streaming/SSE contract and the FFI boundary are preserved.
- The cloud runtime's deep codex integration (`CodexSandbox`) is unchanged in shape;
  `Browser` follows the same "constructed by the runtime, injected via
  `with_handlers`" pattern.
- New tool families can be added in the future by implementing `ToolHandler` only.
