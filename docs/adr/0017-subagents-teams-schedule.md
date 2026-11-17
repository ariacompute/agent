# ADR-0017 — Sub-agents, agent teams and scheduled jobs

Status: Accepted.

## Context

Multi-agent work needs three different things: delegation (a child agent doing one
sub-task), coordination (several workers sharing a task graph) and time (work that
happens without a caller). They have different isolation and duplication risks, so
they are three seams rather than one "multi-agent" switch.

## Decision

### Sub-agents (`subagents` seam)

* Providers: in-process fork (`InProcessSubagents`) and delegation to an external
  runtime. Both report `SubagentHandle { id, session, parent_session }`.
* A child session id is namespaced: `<parent>::sub::<id>` (`seams::child_session`
  / `seams::parent_session` in `agent-core`, shared by the loop, providers and the
  cloud so everyone spells and parses it the same way). The child keeps its own
  event stream — it never interleaves with the parent — while remaining traceable
  and replayable from the parent.
* Concurrency (`DEFAULT_MAX_CONCURRENT = 4`) and step count are capped; exceeding
  the cap is `Denied`, not a queue.
* A child runs through the ordinary one-shot loop, so it records its own
  `turn_started` / `title_set` / terminal events. Titles therefore exist for child
  sessions too, and the cloud resolves them through the parent's ownership rather
  than a session row.
* `wait(id)` returns the child's final text and caches the outcome (idempotent,
  never re-runs). A failing child reports the failure; providers that cannot
  report output leave `wait` unimplemented and say so.

### Teams (`team_*` tools)

* A team is a **validated DAG**: empty names, empty task lists, duplicate ids,
  unknown dependency ids, self-dependencies and cycles are all refused *before* the
  team is stored (Kahn's algorithm for cycle detection).
* Tasks move `pending → claimed → done`; `claim` is dependency-gated and refuses a
  task that is already claimed, so two workers cannot start the same task, and
  `complete` requires the claim. `release` returns a task to the pool.
* The graph shape never changes after creation; only task state does. Postgres
  stores the DAG as one JSON document so a claim/complete pair runs under a single
  row lock (no interleaving, no N+1). Every row carries `principal_id`; an id
  owned by another tenant reads as `NotFound` and writes as `Denied`.

### Scheduled jobs (`schedule` seam)

* Triggers are `every_secs` or `at_ms` (epoch ms). No cron parser: a cron
  expression would pull `chrono`/`cron` for a feature nobody asked for.
* The ticker **advances `next_run_ms` before running** the job, so a crash or a
  second replica cannot fire the same instant twice; a one-shot entry is disabled
  after firing. A past one-shot still fires on the next tick (a reminder created
  offline must not vanish).
* `try_lock` on the ticker keeps one process from overlapping ticks; a failing job
  is logged and does not stall the remaining entries.
* Rows carry `principal_id`; ids are unique per tenant and a conflict across
  tenants is `Denied`.

## Consequences

* Child work cannot corrupt the parent transcript; teams cannot deadlock on a
  cycle; jobs cannot double-fire.
* The cloud can expose teams/jobs over HTTP without new semantics — the routes are
  thin wrappers over the same seams.
* Multi-agent orchestration is opt-in per deployment: an unconfigured provider
  means the tools are simply absent.
