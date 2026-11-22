# ADR-0025 — One governed egress boundary: policy, credentials, projection

- Status: accepted
- Date: 2026-11-19

## Context

An agent decides what to do *at runtime*, so a gateway that only guards an
inbound "front door" cannot govern it. The decisions that need governing are the
ones that become real: which host is contacted, which credential is attached, and
how much of a tool's output reaches the model.

Three mechanisms are usually built as three unrelated features. That produces
gaps: a policy denial that is not in the same audit stream as the request that
was actually sent, or a credential path that a different transport bypasses.

## Decision

`aria-agent-governance` provides **one** outbound interception point,
`EgressGateway`, which performs three steps in a fixed order for every
connection:

1. `policy.decide(host)` — may it leave?
2. `credentials.substitute(host, path, headers)` — plaintext HTTP only.
3. `audit.record(...)` — for **both** outcomes, with the policy revision.

The seam pairs are `egress` (policy) and `credentials` (substitution); both are
resolved once at assembly and never per tool call.

### Two transports, two trade-offs

| Client | Transport | Credential substitution |
|---|---|---|
| `web_*` tools | in-process HTTP via the gateway | yes |
| sandbox traffic | `HTTP_PROXY`/`HTTPS_PROXY` → local proxy | plaintext HTTP only |

`CONNECT` requests (HTTPS) are **decided and audited, then blind-tunnelled**.
There is deliberately no TLS interception:

* no self-signed CA to distribute;
* no new trust-store single point (the workspace pins `webpki-roots >= 1.0.10`
  after the eMudhra/emSign CA compromise);
* the honest cost is that a `${credential}` placeholder inside a TLS body is
  **not** substituted. Deployments that need that must terminate TLS themselves
  and forward plaintext to the proxy.

### Enforcement modes

| `AGENT_EGRESS_ENFORCEMENT` | Behaviour |
|---|---|
| `proxy` (default) | proxy env injected into every sandbox; all traffic reaches the gateway |
| `network` | proxy container on an internal Docker network (strongest, needs container wiring) |
| `off` | no interception; requires `AGENT_ALLOW_UNENFORCED_EGRESS=1` and emits an `unenforced_egress` audit event |

There is no silent downgrade: an unknown mode fails boot.

## Consequences

* **Deny-by-default** is the only default. `allowlist` with an empty host list is
  a configuration error (use `deny_all` to block everything).
* Every denial is fed back to the model with the policy revision, so the fix is
  "add the host", not "retry until it works".
* `web_*` and `shell` can no longer diverge: the shell path is covered by the
  proxy, not by a second check in the tool.
* A `CONNECT` tunnel is opaque after the handshake. Anything that must be
  inspected must be sent as plaintext HTTP to the proxy.
