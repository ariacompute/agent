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
6. **Per-tenant `ActiveHarness`.** Each tenant owns its own hot-swappable
   `ActiveHarness`, lazily created from baseline (or its last winning harness
   under `REEF_DIR/tenants/<pid>`, versioned in its own isolated git repo). A
   tenant's `/reef/evolve` win hot-swaps **only that tenant's** served harness —
   one tenant's self-improvement can no longer regress another's. See
   `TenantHarnessRegistry` + `AppState::harness_for`.
7. **Key-lookup cache with revocation TTL.** A process-wide `KeyCache`
   (identity map `key_hash → resolved principal`, TTL default 60s via
   `API_KEY_CACHE_TTL_SEC`) short-circuits the per-request Postgres point query.
   Only **valid** lookups are cached; revocation (`DELETE /v1/api-keys/:id`)
   actively purges the entry, so a revoked key stops working immediately. The TTL
   is a safety net for missed purges (e.g. multi-instance deploys).
8. **Per-tenant `agents`/`runs` isolation.** `agents` now carry a `principal_id`;
   `create_agent` / `get_agent` / `agent_name` scope by it (admins see all,
   tenants only their own). `runs` already carried `principal_id` for audit. SQL in
   `ensure_schema` adds the column idempotently.
9. **SDK/FFI principal wiring.** The native SDK surface (`ariacompute-agent`)
   gains `create_agent_for_tenant(tenant_id, config)`, which namespaces the
   agent's local memo store under `ARIA_MEMO_DIR/tenants/<tenant_id>` (matching
   the cloud's per-tenant memo sharding). Generated Swift/Kotlin bindings
   regenerated via `just ffi`.

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

- (None outstanding from the original multi-tenant plan — items 6–9 above are
  now implemented.) Future hardening could add per-instance cache refresh
  coordination (Redis) so revocation propagates instantly across a fleet, and a
  tenant quota/rate-limit layer keyed by `principal_id`.

## References

- OpenAI Agents API auth overview (project keys + scopes, app-layer end-user
  identity, no key-in-sandbox).
- `crates/aria-agent-cloud/src/main.rs` — `Principal`, `TenantStoreRegistry`,
  `require_auth`, `/v1/api-keys` handlers.
- `migrations/0002_api_keys.sql`.
- AGENTS.md rules #1, #5, #7.
