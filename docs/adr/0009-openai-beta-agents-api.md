# ADR-0009 — Cloud API: OpenAI beta Agents resource surface

* Status: Accepted
* Date: 2026-09-17
* Supersedes: the `/v1/sessions/:id/runs[/stream]` contract described in
  ADR-0007 §streaming and the frozen contract in `AGENTS.md` rule 10.

## Context

`aria-agent-cloud` served a bespoke REST surface (`POST /v1/agents`,
`POST /v1/sessions/:id/runs`, `POST /v1/sessions/:id/runs/stream`) whose SSE
frames were OpenAI **Responses**-shaped (`response.*`). OpenAI has since
published a dedicated **beta Agents** resource
(`/v1/agents`, `/v1/agents/sessions`, `…/events`, `…/events/stream`) with its own
`agent.*` streaming envelope. Keeping both shapes means maintaining a private
dialect and a translation layer forever.

## Decision

1. **Adopt the beta Agents surface as the only API.** Implemented:
   * `POST|GET /v1/agents`, `GET|POST|DELETE /v1/agents/{agent_id}`
   * `POST|GET /v1/agents/sessions`, `GET|POST|DELETE /v1/agents/sessions/{session_id}`
   * `POST /v1/agents/sessions/{session_id}/events` (blocking turn)
   * `POST /v1/agents/sessions/{session_id}/events/stream` (SSE turn)
   * read-only `…/items`, `…/turns`, `…/subagents`
   * `501 Not Implemented` for `vaults`, `agents/environments` (+files/templates)
     and `agents/sessions/{id}/artifacts`
2. **Requests accept `OpenAI-Beta: agents=v1`.** The header is accepted (and
   echoed by the SDKs); it is not required, so existing Aria clients keep working.
3. **Streaming envelope is `agent.*`.** `agent.turn.created` →
   `agent.turn.in_progress` → `agent.turn.item.added` / `agent.turn.item.done` →
   `agent.turn.output_text.delta` → `agent.turn.output_text.done` →
   `agent.turn.completed`, with `agent.turn.failed` on error. `agent.turn.completed`
   / `agent.turn.failed` is the **only** terminal event; there is no `data: [DONE]`
   sentinel. Every frame carries `event_id`, `session_id`, `turn_id` and a
   monotonic `sequence_number`.
4. **Strict switch, no compatibility layer.** The old `/v1/sessions*` routes and
   the `response.*` frames are removed, along with the Reef receipt header
   `x-reef-agent-record-id` (see ADR-0006 → feature removed). Downstream
   consumers must migrate; the migration points are recorded in
   `docs/followups/`.
5. **Subagents are always empty.** The runtime executes a single agent loop, so
   `…/subagents*` returns empty lists / 404 rather than fabricating data.

## Consequences

* Downstreams (playground, cockpit, marketing copy) break until they migrate —
  accepted, and tracked in `docs/followups/*.md`.
* Any OpenAI client that speaks the beta Agents dialect can target this service.
* `agent_id` / session ids use OpenAI-ish prefixes (`agent_<hex>`, `sess_<hex>`,
  `turn_<hex>`, `item_<hex>`).
