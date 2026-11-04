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
| `codex/**` | git submodule, not this repository's source |
| `crates/aria-agent-ffigen/**` | binding *generator* binary; its output is verified by `just ffi` and the committed Swift/Kotlin sources |
| `crates/aria-agent-cloud/src/main.rs` | process entrypoint: `main`/`run`/`cmd_setup*`/`cmd_serve`, signal handling and the external `codex_sandbox` wiring |
| `crates/aria-agent-cloud/src/store/pg.rs` | Postgres metadata backend: needs a live database. The `DATABASE_URL`-gated tests stay in place (`#[tokio::test]` + early `return` when unset) so a real deployment can still exercise them — they never fake a result |
| `crates/aria-agent-cloud/src/context/pg.rs` | pgvector context backend, same reasoning as above |

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
   mapping stays a single exercised line.
3. If a branch is truly unreachable, remove it or document it here — do not pad
   the exclusion list with business logic.
