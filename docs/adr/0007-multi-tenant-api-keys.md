# 0007 — Multi-tenant API keys for agent-cloud

- Status: Accepted (2026-09-14)
- Deciders: aria-compute
- Context: `aria-agent-cloud` auth gate + per-tenant data isolation.

## Context

`aria-agent-cloud` originally authenticated every caller with a single shared
environment variable, `AGENT_CLOUD_API_KEY`. All callers presented the same key
and the service kept **one** global `memo` / `reef records` / `reef feedback`
store — there was no way to tell *who* was calling, and no way to keep one user's
conversational context or learning logs separate from another's.

We evaluated the design against the OpenAI **Agents API** auth model
(https://developers.openai.com/api/docs/guides/agents-api/overview):

- OpenAI authenticates at the **project / API-key level** with explicit scopes
  (`api.agents.read`, `api.agents.write`, `api.responses.write`). It does **not**
  do end-user-level auth at the key layer.
- Distinguishing individual end users is explicitly an **application-layer**
  responsibility (the app passes per-request user identifiers / owns tenancy).
- Key-hygiene rule from OpenAI: never place the API key inside the agent's
  sandbox.

Conclusion: "supporting different users" must be built as a **principal (tenant)
identity layer** on top of the service, not borrowed from OpenAI.

## Decision

Introduce a multi-tenant API-key system:

1. **Principal identity.** A resolved `Principal { id, kind: Admin|User, scopes }`
   is attached to every request by the auth middleware and exposed as an axum
   extractor. Its `id` is also the sled path shard for tenant data.
2. **Key metadata in Postgres** (`api_keys` table): `id`, `key_prefix`,
   `key_hash` (sha256, **never the plaintext**), `principal_id`, `label`,
   `scopes`, `created_at`, `revoked_at`. Postgres remains metadata-only, per
   AGENTS.md rule #1.
3. **Bootstrap admin key.** `AGENT_CLOUD_API_KEY` is retained as the
   backward-compatible bootstrap/admin key. Presenting it resolves to the Admin
   principal. This keeps existing single-key deployments working unchanged.
4. **Self-serve key management.** `POST/GET /v1/api-keys`,
   `DELETE /v1/api-keys/:id` — admin-scope only — let an operator mint and
   revoke per-tenant keys (OpenAI-style project keys). The plaintext key is shown
   **once** at creation.
5. **Per-tenant data isolation.** `memo`, `reef records`, and `reef feedback`
   are sharded by `principal_id` under `MEMO_DIR/<pid>`,
   `REEF_DIR/records/<pid>`, `REEF_DIR/feedback/<pid>`. The admin/bootstrap
   principal's stores keep the **legacy root paths** so existing data is not
   orphaned. See `TenantStoreRegistry` (lazy open + cache).
6. **Shared harness.** `ActiveHarness` stays fleet-wide; `/reef/evolve` is bound
   to the caller's tenant records/feedback but still hot-swaps the shared
   `active`.

### Open vs closed mode

- `AGENT_CLOUD_API_KEY` unset **and** no valid presented key → open mode, resolves
  to the `default` tenant (preserves the old "run open" dev convenience).
- `AGENT_CLOUD_API_KEY` set → auth is on; a request without a valid key (env or
  `api_keys` row) gets `401`.

## Consequences

- Positive: callers are distinguishable; context/learning logs are isolated;
  operators can issue/revoke per-tenant keys and scope them with `read`/`write`/
  `admin`.
- Positive: backward compatible — single-key deployments keep their data and the
  old gate semantics.
- Negative: every authenticated request does one Postgres point query by
  `key_hash` (indexed, cheap). Stores are opened lazily on first use per tenant.
- Security: keys are hashed (sha256) before storage; plaintext is returned only
  once; revoked keys are rejected; `principal_id` is path-sanitized
  (`[A-Za-z0-9_-]`, ≤64) to prevent sled path traversal. No key or raw user
  content is logged (AGENTS.md rule #5).

## Unresolved / future work

- Per-tenant `ActiveHarness` (each tenant evolves its own served harness) instead
  of one fleet-wide harness. Currently the winning harness is global.
- Optional in-memory cache of `key_hash → principal` with revocation TTL to cut
  the per-request DB lookup further.
- Per-tenant `agents`/`runs` isolation (today `agents` are global definitions;
  `runs` carry a `principal_id` for audit only).

## References

- OpenAI Agents API auth overview (project keys + scopes, app-layer end-user
  identity, no key-in-sandbox).
- `crates/aria-agent-cloud/src/main.rs` — `Principal`, `TenantStoreRegistry`,
  `require_auth`, `/v1/api-keys` handlers.
- `migrations/0002_api_keys.sql`.
- AGENTS.md rules #1, #5, #7.
