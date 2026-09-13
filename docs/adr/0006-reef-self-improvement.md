# ADR 0006: reef-style self-improvement (record → feedback → evolve → version → hot-serve)

## Status
Accepted.

## Context
We want the agent platform to continually improve itself, inspired by
[Human-Agent-Society/reef](https://github.com/Human-Agent-Society/reef): record
every turn, bind feedback to it, evolve the agent's **harness** (system prompt +
skills + rules), keep the winner, version it in Git, and hot-serve it back
without restarting the process.

The harness is the unit of self-improvement. It must be:
* recorded per turn (so outcomes can be attributed to a specific harness version),
* evolvable by a **local model engine** (`agent_core::ModelClient`, the
  "local engine FFI") rather than a remote training stack,
* versioned so improvements are auditable and rollbackable,
* hot-swappable so live traffic uses the winner immediately.

## Decision
* Add a new crate **`agent-reef`** that owns the self-improvement loop
  (records, feedback, harness files, Git versioning, evolution engine). It
  depends **only** on `agent-core` (for `ModelClient` / `Harness` /
  `ActiveHarness`); `agent-core` does **not** depend back on `agent-reef`.
* **`Harness` / `Skill` / `Rule` / `ActiveHarness` live in `agent-core`** (the
  `Agent` consumes the active harness directly; `agent-cloud` shares one
  `ActiveHarness` across all agents). `ActiveHarness` is an `RwLock<Arc<Harness>>`:
  reads clone an `Arc` (O(1), no async), writes atomically swap the served
  harness — the hot-serve mechanism.
* **Records and feedback are stored in a local sled store** (`agent-reef`,
  `SledRecordStore` / `SledFeedbackStore`), deliberately **not** in Postgres and
  **not** in memo:
  * Postgres (`agent-cloud`) keeps only `agents` / `runs` metadata (ADR 0004).
  * memo keeps only conversational / long-term context (ADR 0004). Records are
    *learning* logs, not context, so they stay out of both.
* **Git versioning** lives under a `.reef/` directory of Markdown harness files
  (`system_prompt.md`, `skills/*.md`, `rules/*.md`). We shell out to the system
  `git` (`std::process::Command`) — no heavy `git2` dependency — committing and
  tagging each winning harness as `reef@<n>`.
* **`agent-cloud`** records every turn and returns the receipt as the
  `x-reef-agent-record-id` response header; adds `POST /reef/report` (bind
  feedback, references must name existing records), `POST /reef/evolve` (run the
  engine), and `GET /reef/versions` (list committed versions + `baseline`). On
  boot it loads the last winning harness from `.reef/` (falling back to the
  baseline) and shares it via `ActiveHarness`.
* **Selection is fail-closed.** The engine scores a candidate against the
  baseline using the feedback signal plus a small reward for added
  skills/rules; it adopts the candidate **only if it is strictly better**, and a
  model error leaves the served harness untouched. Git versioning is best-effort:
  if `git` is unavailable the winner is still hot-served and reported as version
  `0`, with a warning logged — never crashing the service.

## Consequences
* The agent can improve its harness over time from real traffic + feedback, with
  a readable, rollbackable Git history, without restarts.
* Sensitive conversation text stays out of Postgres/memo; records are a separate
  local store.
* The `agent-sdk` / `agent-ffigen` FFI surface is **unchanged** this milestone
  (no drift-check breakage); `/reef/*` is exposed only over HTTP for now.
* Requires `git` on the host for versioning (degrades gracefully without it).
