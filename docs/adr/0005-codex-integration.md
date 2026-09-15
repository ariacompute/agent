# ADR 0005 — Deep compile-integration of codex `sandboxing` / `memories` crates

## Status

**Active — deep integration validated via the `ariacompute/codex` fork.** The
named upstream blocker (`rama-http` failing to compile) no longer reproduces
under current stable Rust (re-tested 2026-09-15, `rustc 1.98.0`). End-to-end
validation is complete for the agent-side pull-in: `aria-agent-cloud`
(publish = false) depends on `codex-sandboxing` through a git dependency on the
`ariacompute/codex` fork (tag `root-workspace-v1`, which adds the repo-root
workspace manifest), and `cargo check -p aria-agent-cloud` pulls codex's full
graph (`rama-*`, `mxc`, `tungstenite` forks) transitively. The codex-backed
`CodexSandbox` is injected into `aria-agent-core`'s `Agent` via `with_sandbox`;
the published `aria-agent-sandbox` keeps the self-contained Docker/Kata/Cube
seam for SDK users and offline tests.

## Context

The platform is meant to integrate OpenAI's `codex` repo (a git submodule under
`codex/`, main workspace `codex/codex-rs/`) at the *crate* level:

* `aria-agent-sandbox` should build on codex's concrete `sandboxing` /
  `linux-sandbox` crates (real `SandboxType` / `SandboxManager` primitives).
* `aria-agent-memo` should reuse codex's concrete `memories/read` +
  `context-fragments` crates (real fragment model + memory-injection helpers).

### Cargo cross-workspace constraint (resolved via a codex fork)

codex's Rust workspace lives at `codex/codex-rs/`; the repo *root* has no
`Cargo.toml`. Two consequences:

* A bare `git = "https://github.com/openai/codex"` dependency fails to resolve —
  cargo requires the git repo root to carry a manifest, and the nested
  `codex-rs` workspace is not reachable from the root.
* A `path` dependency into that workspace is absorbed by the consuming
  workspace, which breaks the depended-on crate's `*.workspace = true`
  inheritance (e.g. `anyhow`); and `git` + `path` on the same dep is forbidden.

The fix is to maintain a **fork** (`ariacompute/codex`) whose repo root *is* a
workspace: promote the `codex-rs` workspace definition up to `Cargo.toml` at the
repo root, prefix every member + internal `path` dependency with `codex-rs/`, and
delete the old `codex-rs/Cargo.toml`. That commit is tagged `root-workspace-v1`.
The dependency then needs no `path` qualifier:

```toml
codex-sandboxing = { git = "https://github.com/ariacompute/codex", tag = "root-workspace-v1", package = "codex-sandboxing" }
```

(codex's edition is already `2024` via `[workspace.package]`, so no edition bump
is required in the consumer.)

Because codex-rs patches crates.io forks (`tokio-tungstenite` `proxy` feature
via `codex-otel`), the consumer must replay those patches:

```toml
[patch.crates-io]
crossterm       = { git = "https://github.com/openai-oss-forks/crossterm", rev = "45fecb9508105988f42fe6ff0441783ed3717f92" }
tokio-tungstenite = { git = "https://github.com/openai-oss-forks/tokio-tungstenite", rev = "0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186" }
tungstenite     = { git = "https://github.com/openai-oss-forks/tungstenite-rs", rev = "4fffad30fe373adbdcffab9545e9e9bf4f2fc19f" }
```

## Blocker (resolved — re-tested 2026-09-15)

Both crate families transitively depend on `rama-http 0.3.0-alpha.4` (from
crates.io, identical to codex's pinned `Cargo.lock`).

> **Historical blocker.** At the time this ADR was written, that published crate
> did **not compile** under the then-current stable Rust:
>
> ```
> error[E0308]: mismatched types
>    --> .../rama-http-0.3.0-alpha.4/src/layer/har/spec.rs:104:36
> 104 |             other => Self::Unknown(format!("{other:?}")),
>     |                                    ^^^^^^^^^^^^^^^^^^^^ expected `Box<str>`, found `String`
> ```
>
> Because `rama-http` was unreachable without a compatible release, *no* codex
> crate compiled as a dependency, breaking `cargo build --workspace`.

**Re-test (2026-09-15, stable `rustc 1.98.0`).** With the *codex-locked* graph
(the whole `rama-*` family pinned to `=0.3.0-alpha.4`, per
`codex/codex-rs/Cargo.lock`) `rama-http 0.3.0-alpha.4` compiles cleanly. Verified
by a scratch crate that produces `librama_http-*.rlib` with **0 errors**; the
original `spec.rs:104` inference error no longer reproduces (newer rustc's
match-arms type inference resolves it). The blocker is therefore **gone** on the
compiler side.

> **Caveat — pin the entire `rama` family.** A naive
> `rama-http = "=0.3.0-alpha.4"` without pinning sibling `rama-*` crates lets
> cargo upgrade `rama-net` / `rama-http-types` / … to the *released* `0.3.0`,
> which is API-incompatible with the alpha.4 source and produces ~139 spurious
> errors (E0053/E0191/E0425/E0599/…). Any integration must pin the **whole**
> `rama-*` family to `=0.3.0-alpha.4`, exactly as codex's `Cargo.lock` does.

## Decision

* Keep `codex` as a submodule (per ADR 0001), but the *compile* dependency on
  codex's concrete crates is satisfied via a **git dependency on the
  `ariacompute/codex` fork** (tag `root-workspace-v1`), not a path into the
  submodule. The fork's repo-root workspace is what lets `package = "codex-*"`
  resolve (see above).
* The codex-backed tool-execution backend (`CodexSandbox`, using codex's
  concrete `sandboxing` `SandboxType` / `SandboxManager` primitives) lives in
  `aria-agent-cloud` (publish = false) and is injected into `aria-agent-core`'s
  `Agent` via `with_sandbox`; the published `aria-agent-sandbox` keeps the
  self-contained Docker/Kata/Cube `Sandbox` seam as a fallback for SDK users and
  offline tests (never used on the codex deployment path).
* `aria-agent-memo` remains the context store (sled); codex's `memories` /
  `context-fragments` crates are read-only helpers and are not pulled in as a
  storage backend.
* The git-dep + `[patch.crates-io]` forks + full `rama-*` pin recipe is the
  prescribed way to enable the deep integration and is validated end-to-end (see
  Status).

## Consequences

* The build is runnable and testable today; `aria-agent-cloud` compiles against
  the real codex `sandboxing` crate transitively, and its deep-integration test
  (`codex_sandbox_exec_runs_command_through_codex_manager`) exercises codex's
  `SandboxManager` end-to-end.
* The `rama-http` compile blocker is resolved on the compiler side (re-tested
  2026-09-15), and the full `rama-*` family is pinned to `=0.3.0-alpha.4` so the
  released `0.3.0` siblings are not pulled in.
* The deep compile-integration of codex's concrete crates is **active**, via the
  `ariacompute/codex` fork; the fork must keep `root-workspace-v1` (or a
  successor tag) in sync with the pinned submodule commit.
