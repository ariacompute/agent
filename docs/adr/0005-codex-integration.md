# ADR 0005 — Deep compile-integration of codex `sandboxing` / `memories` crates

## Status

**Blocked (environmental).** The mechanism is defined; it cannot be enabled in
this workspace until an upstream dependency is fixed.

## Context

The platform is meant to integrate OpenAI's `codex` repo (a git submodule under
`codex/`, main workspace `codex/codex-rs/`) at the *crate* level:

* `agent-sandbox` should build on codex's concrete `sandboxing` /
  `linux-sandbox` crates (real `SandboxType` / `SandboxManager` primitives).
* `agent-memo` should reuse codex's concrete `memories/read` +
  `context-fragments` crates (real fragment model + memory-injection helpers).

### Cargo cross-workspace constraint

A path dependency into another Cargo workspace is auto-absorbed by the
consuming workspace, which breaks the depended-on crate's `*.workspace = true`
inheritance. Adding an empty `[workspace]` table to the codex crate makes it an
invalid root. The correct mechanism is therefore a **git dependency pinned to
the submodule's commit**:

```toml
codex-sandboxing     = { git = "https://github.com/openai/codex", rev = "<submodule commit>", package = "codex-sandboxing" }
codex-linux-sandbox  = { git = "https://github.com/openai/codex", rev = "<submodule commit>", package = "codex-linux-sandbox" }
codex-memories-read  = { git = "https://github.com/openai/codex", rev = "<submodule commit>", package = "codex-memories-read" }
codex-context-fragments = { git = "https://github.com/openai/codex", rev = "<submodule commit>", package = "codex-context-fragments" }
```

Because codex-rs patches crates.io forks (`tokio-tungstenite` `proxy` feature
via `codex-otel`), the consumer must replay those patches:

```toml
[patch.crates-io]
crossterm       = { git = "https://github.com/openai-oss-forks/crossterm", rev = "45fecb9508105988f42fe6ff0441783ed3717f92" }
tokio-tungstenite = { git = "https://github.com/openai-oss-forks/tokio-tungstenite", rev = "0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186" }
tungstenite     = { git = "https://github.com/openai-oss-forks/tungstenite-rs", rev = "4fffad30fe373adbdcffab9545e9e9bf4f2fc19f" }
```

## Blocker

Both crate families transitively depend on `rama-http 0.3.0-alpha.4` (from
crates.io, identical to codex's pinned `Cargo.lock`). That published crate does
**not compile** under current stable Rust:

```
error[E0308]: mismatched types
   --> .../rama-http-0.3.0-alpha.4/src/layer/har/spec.rs:104:36
104 |             other => Self::Unknown(format!("{other:?}")),
    |                                    ^^^^^^^^^^^^^^^^^^^^ expected `Box<str>`, found `String`
```

Because `rama-http` is unreachable without a compatible release, *no* codex
crate compiles as a dependency in this environment. Attempting the integration
therefore breaks `cargo build --workspace`.

## Decision

* Keep `codex` as a submodule (per ADR 0001).
* `agent-sandbox` and `agent-memo` remain **self-contained** integration seams
  whose shape mirrors codex's architecture (pluggable `Sandbox` with a Docker
  default; a `MemoStore` with local sled persistence — see ADR 0004).
* The git-dep + patch recipe above is the prescribed way to enable the deep
  integration once `rama-http` ships a compiling release (or a team reproduces
  codex's full locked graph). Until then, the codex crates are **not** in the
  dependency graph so the workspace stays green and `cargo test --workspace`
  passes.

## Consequences

* The build is runnable and testable today.
* The "deep compile-integration" of codex's concrete crates is deferred, not
  abandoned; the seam and the integration recipe are in place.
