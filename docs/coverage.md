# Line-coverage gate

The workspace is held at **100% coverage of the included lines** by
`scripts/coverage.sh` (`just cov-check`), which runs `cargo-llvm-cov` over the
whole workspace and then checks the lcov `DA:` records.

"Included" = every measured source line minus the two categories that carry no
statement of their own: closing-delimiter artifacts and `// LCOV_EXCL_LINE`
opt-outs (see below). A run with no opt-outs reads:

```
lines measured: 37225/37240 (99.96%)
closing-delimiter artifacts: 15 (no statement; not counted)
lines included: 37225/37225 (100.00%)
OK: every included line is covered (or explicitly excluded).
```

The fifteen artifacts are all closing braces whose block did run: the statement
on the line above each carries a non-zero hit count, and llvm still emits a
zero-count gap region at the delimiter.

* `a2a/executor.rs` (5): the `Done` arm of the event stream and the four
  `if let` blocks in `fail` and `cancel`.
* `a2a/rpc.rs` (2): the push-URL check in `message/stream`, and the
  `metadata.answer` arm of `resume_input`.
* `rsi/ledger.rs` (2): the two eviction `if let` blocks (session capacity and
  record window) — both were entered by fresh-key and over-window batches.
* `rsi/validate.rs` (2) and `core/seams/tools.rs` (1): `if let` assertions in
  tests, where the line above asserts the variant, so the body always runs.
* `dsh/sandbox.rs` (1) and `governance/proxy.rs` (2): two `tokio::spawn` bodies
  of the test doubles and the never-returning `pending_dial` helper.

They are printed for transparency. No test can enter a delimiter, and replacing
the construct would change the code being measured rather than cover it.

There are no per-line opt-outs left. The last one was `HttpPushNotifier::new`:
`reqwest::ClientBuilder::build` fails only when the TLS backend cannot
initialise, which cannot be provoked hermetically, and booting without a client
would turn a push target into a silent no-op. The mapping is now isolated in
`HttpPushNotifier::build`, which takes the constructor as an argument, so
`a_client_that_cannot_be_built_is_reported_not_swallowed` pins the error text —
and the `Ok` arm still runs through the real `new()` — without faking a broken
platform.

```bash
just cov-check        # gate: fails on any uncovered included line
just cov              # report only: writes lcov.info
just pg-test          # the Postgres/pgvector suite; needs DATABASE_URL
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
| `crates/aria-agent-cloud/src/store/pg.rs` | Postgres metadata backend: needs a live database, so it cannot sit in an offline gate. It is **not** untested: `just pg-test` (the `postgres` CI job) runs the whole pg suite against a real `pgvector/pgvector:pg16`, which is how the `SUM(...)::BIGINT` decoding bug in `usage()` was found. See “Why the pg files stay out of the line gate” below |
| `crates/aria-agent-cloud/src/context/pg.rs` | pgvector context backend, same reasoning and same suite |
| `crates/aria-agent-cloud/src/store/pg_orchestration.rs` | Postgres teams/schedule/title backends; covered by the real-database suite as well as by the in-memory providers in `api_tests.rs` |
| `crates/aria-agent-cloud/src/store/pg_rsi.rs` | Postgres RSI backends (reports, pending proposals, revision-guarded playbooks, insert-only re-run receipts); the real-database suite exercises the `409` revision guard, the `422` adaptation rule and the unique-index conflict mapping |
| `crates/aria-agent-cloud/src/pg_testkit.rs` | the fixture those tests share (one pool per test, schema applied once behind a process-wide lock). Its skip arms only run without a database, so it travels with the group above |
| `crates/aria-agent-cloud/src/a2a/pg.rs` | Postgres A2A task store: the SQL needs `DATABASE_URL`, so it joins the group above rather than the offline gate. It is **not** unpinned: the module has DB-gated tests for the column round-trip, the CAS transition, the per-task event sequence, tenant scoping, push config/receipt and `fail_orphans`, and `MemoryA2aTasks` pins the same contract offline |
| `crates/aria-agent-tools/src/terminal.rs` (`pty`) | optional `pty` provider needs a real terminal device; `scripts/coverage.sh` excludes this file so enabling the feature cannot make the gate depend on a TTY. The default build keeps `pty` off. |
| `crates/aria-agent-dsh/src/transport.rs` | child-process plumbing: piped stdio "unavailable" is unreachable with `Stdio::piped`, and write/flush/read `EIO` plus `start_kill`/`try_wait` failures cannot be produced hermetically. Exercised end-to-end by `just dsh-smoke` (scripted child) and `just dsh-e2e` (real dsh). |

`crates/aria-agent-sandbox/src/egress_net.rs` used to be excluded as “needs a
live daemon”. It is now **inside** the gate: its test module runs a scripted
Engine API over loopback, so `ensure` (create network → create proxy → connect →
start), `probe`, `health`, `resolve_peer` and `control_networks` all exercise the
real `bollard` request/response path. Writing those tests found a real defect:
`plan` asked for `ConnectProxyToControl`/`ConnectProxyToEgress` even right after
`CreateProxy` had already attached the container, and Docker answers
`403 endpoint ... already exists in network` to the second `connect`, so a fresh
bring-up would have failed on itself.

`crates/aria-agent-dsh/src/rpc.rs` is **inside** the gate: the request/response,
timeout, server-request, notification, dead-pipe, read-error and dropped-channel
arms are unit-tested, and the test doubles' own arms are exercised by
`the_test_transports_cover_their_unused_arms`, so no whole-file exclusion is
needed for the demultiplexer. `just dsh-smoke` additionally exercises it against
a real child process.

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

## Why the pg files stay out of the line gate

The pg backends have a real suite (`just pg-test`, `DATABASE_URL` required,
`postgres` CI job), but they do not join the **line** gate, for a measurement
reason rather than a testing one: on `sqlx` call chains llvm reports zero-count
*gap regions* for lines that demonstrably ran. A concrete example, from a run
with a live database where the assertion `list_checkpoints(...).len() == 1`
passed:

```
   240| 2|        sqlx::query("INSERT INTO sandbox_checkpoints ...")      ^0
   253| 0|            .execute(&self.pool)
   255| 0|        Ok(())
   264| 0|        .fetch_all(&self.pool)
   267| 1|            .map(|row| {
```

`.fetch_all` cannot be unexecuted while the `map` below it ran, and `Ok(())`
cannot be unexecuted in a function that returned `Ok`. The gate would flag those
lines forever, and the only way to silence them is to rewrite the SQL chains into
unnatural single-line shapes. The behaviour those lines encode *is* covered — by
`just pg-test` against a real server, which is the stronger check (it found the
`NUMERIC` vs `BIGINT` decode bug).

Everything that is *offline testable* stays inside the gate: `tools/web.rs` used
to be excluded for “needs live HTTP”, but its only unexecuted line was
`host_of("http://user@:8080/x")`, so it is now measured like everything else.

## Test doubles

No test reaches the public internet or a live database:

| Dependency | Double |
| --- | --- |
| OpenAI-compatible upstream | `wiremock` loopback server (`aria-agent-core`, `upgrade`) |
| Docker daemon | hand-rolled Engine API stub over TCP in `crates/aria-agent-sandbox` (raw-stream `exec` upgrade included) |
| Postgres / pgvector | `store/memory.rs` + `context::MemoryContextStore` for the offline suite; a real `pgvector/pgvector:pg16` behind `DATABASE_URL` for `just pg-test` (one pool per test, schema applied once — the shared `pg_testkit` exists because per-test DDL raced into `23505` on `pg_type`) |
| `aria-memo` CLI | `cli-interop` feature (off by default) — a plain `cargo test` never silently skips |
| Cloud HTTP API | `tower::ServiceExt::oneshot` against `build_router(state)` |
| Gateway HTTP (thinking-filter proxy) | one-shot loopback gateway, plus `ProxyListener` / reader / writer seams for the `bind`/`local_addr`/`accept` and short-read/short-write branches a real socket cannot be made to produce |

Tests that genuinely need an external binary or database stay gated and are
excluded above; they must never assert a fabricated success.

Integration test files (`crates/*/tests/`) are outside this report in any case —
`cargo llvm-cov` leaves test code out unless `--include-tests` is passed — so a
gated suite cannot inflate the number either. Two of them belong to the dsh
sandbox, and both print a skip reason and return when their variable is unset:
`crates/aria-agent-sandbox/tests/e2e_docker.rs` (`ARIA_E2E_DOCKER=1`: duplex
attach, `copy_in`, and a volume that survives a container rebuild) and
`crates/aria-agent-dsh/tests/sandbox_live.rs` (`ARIA_DSH_SANDBOX_E2E=1`: the
containerised `initialize` handshake). Both were run against a real daemon; the
volume test deletes its own named volume on the way out, because a volume is
global state that would otherwise collect run after run.

## Adding new code

1. New logic needs a normal-path **and** an abnormal-path test.
2. A new `map_err(|e| ...)` closure per call site is a coverage trap: format the
   message in one shared helper (`db_err`, `model_err`, `prefixed`) so the
   mapping stays a single exercised line. `LocalForwardProxy` follows this: the
   socket-write and relay failures live in `send_bytes` / `relay`.
   `ThinkingFilterProxy` uses `config_err` / `model_err` the same way, and keeps
   its logging on one line so `rustfmt` cannot split a `tracing::debug!` into
   regions that only run with a subscriber. A fallible constructor follows the
   same rule: `HttpPushNotifier::build` takes the closure that builds the client,
   so the error mapping is one exercised line instead of an opt-out.
3. If a branch is truly unreachable, remove it or document it here — do not pad
   the exclusion list with business logic.

## Per-line opt-out (`LCOV_EXCL_LINE`)

A single statement may opt out of the gate with a trailing marker and a short
reason in the surrounding comment:

```rust
some_call().map_err(|e| report(e))?; // LCOV_EXCL_LINE
```

The gate counts these separately (`excluded by LCOV_EXCL_LINE`) and prints the
number, and `scripts/coverage.sh` still fails on the line next to the marker.
There are currently **no** per-line opt-outs: every error branch in the gate is
covered by a test. The patterns that made earlier opt-outs look unavoidable are
all reproducible hermetically:

* socket `EIO` — `SO_LINGER = 0` on a loopback peer delivers RST, so the proxy's
  read, write and relay failures are exercised;
* dial timeout — the `connect_with` seam accepts the dial future, so a test
  injects one that never resolves and the deadline is reached deterministically
  on every kernel;
* listener failure — the internal `ProxyListener` seam supplies a listener
  whose `local_addr` and `accept` fail;
* atomic replace — pointing the store file at a directory makes the rename fail
  after the temp write succeeds;
* fallible constructor — the build is injected (`HttpPushNotifier::build`), so a
  test hands in the `Err` value and the mapping runs without a broken TLS
  backend.

Use a marker only for a branch that must stay in the code but genuinely cannot
be produced hermetically. Anything that could be tested belongs in a test.

## Closing-delimiter artifacts

llvm emits a region for the closing delimiter of some blocks (`}`, `});`,
`},`) that is never entered even when the block body runs. A line made only of
closing delimiters carries no statement, so the gate reports those as
`closing-delimiter artifacts` instead of failures; an unexecuted statement
inside the same block is still reported on its own line.
