# ADR-0011 — SDK memory context backend switch (cloud / local / both)

* Status: Accepted
* Date: 2026-09-17
* Related: ADR-0010 (context storage), ADR-0008 (SDK ergonomics)

## Context

After ADR-0010 the cloud stores context in Postgres + pgvector and the on-device
SDK used an embedded store. Callers had no way to choose: a native app could not
reach the cloud store at all, and a server-side script could not keep a local
copy for offline use. Products need all three shapes — cloud-only, local-only,
and "write everywhere, read merged".

## Decision

1. **Every SDK exposes the same backend enum**: `cloud` | `local` | `both`
   (JS/TS type, Python `MemoryBackend` enum, Rust `MemoryBackend`, Swift/Kotlin
   string via UniFFI).
2. **Configuration has exactly two levels**:
   * constructor / `Agent` + `Session` configuration (highest priority),
   * per-call override (`memorize(key, value, { backend })` /
     `memorize(key, value, backend="local")`).
   * No environment variable selects the backend (only implementation details
     such as the `aria-memo` binary / db path may be read from the environment).
3. **`local` = aria memo.** The on-device store is the aria memo SQLite format
   (`memories` table, `memo_type` strings, embedding BLOB, `metadata` JSON), so
   `aria-memo list --json` can inspect the same file.
   * Rust / Swift / Kotlin: `aria-agent-memo::MemoContextStore` (rusqlite).
   * Python: direct SQLite through the stdlib `sqlite3` module.
   * JS/TS: the `aria-memo` CLI (`--json`), with an injectable exec seam so
     tests never need the binary.
4. **The previous sled implementation is deleted.** Two on-device formats would
   make "which copy is real" an operational trap; `memo.db` is the single source
   of truth and the CLI can migrate/inspect it.
5. **`both` = write twice, read merged + deduped.** `memorize` writes local first
   (offline-safe) then cloud; a single-side failure is logged and tolerated,
   only a total failure errors. `recall` merges both result sets, dedupes by id
   (or key) and re-ranks with the shared `0.7 * cosine + 0.3 * keyword` scoring.
   `get_by_key` prefers the cloud copy and falls back to local.
6. **Native SDKs gain a cloud backend.** `ariacompute-agent` ships a small
   `CloudContextStore` (reqwest) against
   `/v1/agents/sessions/{id}/memory[?text=&top_k=][/{key}]`, so Rust/Swift/Kotlin
   can select `cloud` or `both` like the JS/Python SDKs.
7. **No fabricated results.** A missing `aria-memo` binary or an unreachable
   cloud raises a clear error (with an install hint); nothing is silently faked.

## Consequences

* `ContextStore` gained `list_session(session, top_k)` so `compact` and the
  memory listing work uniformly across backends.
* The cloud memory endpoints accept unkeyed fragments (`kind` defaults to
  `long_term`), and `GET …/memory` doubles as a listing endpoint (no `text`).
* UniFFI surface changed: `SdkAgentConfig.memory: SdkMemoryConfig`,
  `SdkSession.memorize/recall(key, value, backend?)`, `SdkAgent.memory_backend()`.
  Bindings were regenerated with `just ffi`.
* crates.io publishing is unaffected: `rusqlite` and `reqwest` are registry
  dependencies (no git-only dependency was added).
