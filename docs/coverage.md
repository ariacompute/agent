# Line-coverage gate

The workspace is held at **100% line coverage** by `scripts/coverage.sh`
(`just cov-check`), which runs `cargo-llvm-cov` over the whole workspace and
then checks the lcov `DA:` records.

```bash
just cov-check        # gate: fails on any uncovered included line
just cov              # report only: writes lcov.info
```

## Why lcov records instead of `--fail-under-lines`

`llvm-cov --fail-under-lines` counts *line records*, and macro/generic
instantiations can emit several records for one source line (for example an
`#[async_trait]` expansion or a `sort_by_key` closure). Those duplicates are
reported as missed even when the source line runs, so a file can look 99% while
its annotated source is fully green. The lcov `DA:<line>,<count>` records are
one per source line, which is exactly the metric we promise, so the gate reads
those. The annotated HTML view (`cargo llvm-cov --html`) uses the same mapping.

## Exclusions

Everything else must be covered. The list is deliberately small and each entry
needs a reason that does not depend on "it was hard":

| Path | Why it is excluded |
| --- | --- |
| `crates/aria-agent-ffigen/**` | binding *generator* binary; its output is verified by `just ffi` and the committed Swift/Kotlin sources |
| `crates/aria-agent-cloud/src/main.rs` | process entrypoint: `main`/`run`, signal handling and the Postgres/OpenAI wiring |
| `crates/aria-agent-cloud/src/store/pg.rs` | Postgres metadata backend: needs a live database. The `DATABASE_URL`-gated tests stay in place (`#[tokio::test]` + early `return` when unset) so a real deployment can still exercise them — they never fake a result |
| `crates/aria-agent-cloud/src/context/pg.rs` | pgvector context backend, same reasoning as above |
| `crates/aria-agent-cloud/src/store/pg_orchestration.rs` | Postgres teams/schedule/title backends; the same semantics are covered by the in-memory providers in `api_tests.rs` |
| `crates/aria-agent-tools/src/terminal.rs` (`pty`) | optional `pty` provider needs a real terminal device; `scripts/coverage.sh` excludes this file so enabling the feature cannot make the gate depend on a TTY. The default build keeps `pty` off. |
| `crates/aria-agent-tools/src/web.rs` (`web`) | optional `web` provider needs live HTTP; excluded by `scripts/coverage.sh` for the same reason. The default build keeps `web` off. |
| `crates/aria-agent-dsh/src/transport.rs` | child-process plumbing: piped stdio "unavailable" is unreachable with `Stdio::piped`, and write/flush/read `EIO` plus `start_kill`/`try_wait` failures cannot be produced hermetically. Exercised end-to-end by `just dsh-smoke` (scripted child) and `just dsh-e2e` (real dsh). |
| `crates/aria-agent-dsh/src/rpc.rs` | JSON-RPC demultiplexer: a few arms are only reachable on a torn-down channel at the same instant a call resolves (`oneshot` closed) or inside test-only transports; the request/response, timeout, server-request, notification and dead-pipe paths are unit-tested. `just dsh-smoke` exercises the same code against a real child. The event mapping, pool and loop stay in the gate. |

`scripts/coverage.sh` exports an isolated `XDG_RUNTIME_DIR` and a placeholder
`DOCKER_HOST` unix socket so sandbox socket probes and the Docker client
constructor are covered without a real daemon or runner-specific environment.
Chmod-based negative assertions are not used: IO error mapping is exercised with
directory read/write failures that behave the same under root and non-root.

The cloud's business logic is **not** excluded: `api/*`, `app.rs`, `auth.rs`,
`store/memory.rs`, `store/mod.rs`, `upgrade.rs`, `cli_config.rs`,
`event_envelope.rs`, `types.rs` and `util.rs` are all measured at 100%. That is
possible because the data-access layer is a set of traits (`AgentStore`,
`SessionStore`, `ApiKeyStore`) with an in-memory implementation used by the
tests, while `store/pg.rs` keeps the production SQL.

## Test doubles

No test reaches the public internet or a live database:

| Dependency | Double |
| --- | --- |
| OpenAI-compatible upstream | `wiremock` loopback server (`aria-agent-core`, `upgrade`) |
| Docker daemon | hand-rolled Engine API stub over TCP in `crates/aria-agent-sandbox` (raw-stream `exec` upgrade included) |
| Postgres / pgvector | `store/memory.rs` + `context::MemoryContextStore` |
| `aria-memo` CLI | `cli-interop` feature (off by default) — a plain `cargo test` never silently skips |
| Cloud HTTP API | `tower::ServiceExt::oneshot` against `build_router(state)` |

Tests that genuinely need an external binary or database stay gated and are
excluded above; they must never assert a fabricated success.

## Adding new code

1. New logic needs a normal-path **and** an abnormal-path test.
2. A new `map_err(|e| ...)` closure per call site is a coverage trap: format the
   message in one shared helper (`db_err`, `model_err`, `prefixed`) so the
   mapping stays a single exercised line. `LocalForwardProxy` follows this: the
   socket-write and relay failures live in `send_bytes` / `relay`.
3. If a branch is truly unreachable, remove it or document it here — do not pad
   the exclusion list with business logic.

## Per-line opt-out (`LCOV_EXCL_LINE`)

A single statement may opt out of the gate with a trailing marker and a short
reason in the surrounding comment:

```rust
std::fs::rename(&tmp, &path)
    .map_err(|e| SeamError::Io(format!("replace {}: {e}", path.display())))?; // LCOV_EXCL_LINE
```

The gate counts these separately (`excluded by LCOV_EXCL_LINE`) and prints the
number. Use it only for a branch that must stay in the code but cannot be
produced hermetically — the OS refusing an operation we have to report (a socket
write to a peer that vanished, a dial that times out, an atomic rename that
fails after the write succeeded). Anything that could be tested belongs in a
test, and `scripts/coverage.sh` still fails on the line next to the marker.

## Closing-delimiter artifacts

llvm emits a region for the closing delimiter of some blocks (`}`, `});`,
`},`) that is never entered even when the block body runs. A line made only of
closing delimiters carries no statement, so the gate reports those as
`closing-delimiter artifacts` instead of failures; an unexecuted statement
inside the same block is still reported on its own line.
