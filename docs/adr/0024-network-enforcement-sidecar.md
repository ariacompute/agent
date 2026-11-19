# ADR-0024 — Network enforcement: one internal network, one secretless proxy

- Status: accepted
- Date: 2026-11-19

## Context

ADR-0020 defined three enforcement modes. `proxy` injects `HTTP_PROXY` into the
sandbox, which is only a convention: a binary that ignores the variable — or a
tool that speaks raw sockets — still has a default route. `network` existed as a
validated configuration value with no implementation behind it.

Making it real needs two decisions: how traffic is physically prevented from
leaving, and who owns the decisions once it is.

## Decision

### Topology

One Docker network named `aria-egress` with `internal: true`, one proxy container
on it, and sandboxes created with `network_mode = "aria-egress"`. `internal`
removes the default route, so the only reachable peer is the proxy. The proxy
container is additionally attached to the control plane's Compose network, which
is how it reaches the API.

The proxy is **not** a Compose service: the control plane creates and starts it at
boot, because the network it needs only exists once the service decides to enforce.

### The proxy holds nothing

| Concern | Where it lives |
|---|---|
| policy snapshot | control plane (`egress_policies`) |
| stub rules, secret values | control plane (credential store) |
| audit events | control plane (`audit_events`, one sink) |

The proxy implements the same seams as the in-process gateway but backed by five
RPCs: `whoami`, `policy`, `stubs`, `secret`, `audit`. Because
`EgressGateway::authorize` / `substitute` are unchanged, deny-wins ordering,
`${credential}` rendering and audit field naming cannot drift between the two
deployments.

`CredentialsSeam` is read-only in the proxy: `rotate`/`delete`/`put_stub` return
`Unsupported`. A second writer of the credential store would be a second truth.

### Identity is the peer IP, resolved by labels

1. The control plane writes `aria.tenant_id` / `aria.session_id` labels when it
   creates a sandbox container.
2. The proxy asks `whoami(peer_ip)`; the control plane resolves it by inspecting
   containers on the network and reading those labels.
3. An unknown peer is `404` → the proxy refuses the connection with `403`. There
   is no default tenant.
4. Every subsequent call carries `peer_ip` again; the control plane re-derives the
   principal and rejects a mismatch. A compromised proxy therefore cannot read
   another tenant's policy by guessing an id.

Cache: `(ip)` → identity for 10s, capped at 4096 entries, cleared when full. IPs
are recycled by Docker, so a long TTL would eventually attribute traffic to the
wrong tenant.

### Authentication

`X-Aria-Proxy-Token`, compared in constant time. The token is generated per boot
when `EGRESS_PROXY_TOKEN` is unset, passed to the container via its environment,
kept in memory, never written to disk and never logged. The `/internal/egress/*`
routes are mounted **only** when `AGENT_EGRESS_ENFORCEMENT=network`.

### Fail boot

`main` calls `bootstrap()` before serving. Network creation, proxy creation,
attaching the proxy to both networks, and a running-container probe must all
succeed. Any failure exits non-zero with the next action ("run in a container, or
use enforcement=proxy"). There is no fallback to `proxy` mode: a silent downgrade
is exactly the failure this mode exists to prevent.

## Consequences

* A sandbox has no route out, so an ignored `HTTP_PROXY` fails closed instead of
  leaking.
* The plaintext hop between the control plane and the proxy is inside the control
  network. `CONTROL_PLANE_URL` supports `https://` with a custom CA for deployments
  that do not trust that network; the trade-off is documented rather than hidden.
* `CONNECT` tunnels are decided and then blind-forwarded (ADR-0020): the proxy
  cannot see inside TLS, so `${credential}` substitution applies to plaintext HTTP.
* Cube (`CliSandbox`) refuses a custom network — it has no network flag, and
  pretending would leave a default route. It refuses identity labels for the same
  reason. `docker` and `kata` are supported. `cube run` does take `-e`, so the
  proxy environment is forwarded instead of dropped; without that a cube sandbox
  would run every tool on the direct path.
* Enforcement is structural, not a policy the loop can forget: `Agent::with_*`
  wraps whatever backend it is handed in `NetworkingSandbox` when
  `egress_network` is set. A backend that already carries network/labels (the
  cloud control plane sets them at build time) keeps them, and the decorator only
  fills empty fields, so the double wrap is a no-op.
* `proxy_env` lives in `aria-agent-sandbox`; the governance re-export and the core
  wrapper both use it, so a sandbox cannot end up with a half-configured proxy
  environment.
* Browser containers are covered: the cloud wraps the browser backend's *inner*
  Docker sandbox with `NetworkingSandbox`, so `spawn_browser`'s empty
  `network`/`labels`/`env` are filled in. The browser engine must honour
  `HTTP_PROXY`; if it does not, navigation fails closed (no route) rather than
  leaking.
* Host PTYs are not coverable: `term_*` runs in the control-plane process, not in
  a container. Under `network` enforcement the family is not registered (the
  `ToolSet` drops it and logs a warning); `shell` / `job_*` stay because they are
  sandbox-backed. Deployments that need an interactive terminal must use
  `enforcement=proxy` and accept that its reach is policy-based.
* `web_fetch` / `web_search` and the turn runtime's own model calls execute in
  the control plane behind the same `EgressSeam` (policy + audit), not inside the
  internal network. They are governed by decision, not by routing. The proxy
  publishes no host port on purpose, so a host process cannot borrow it.
* Checkpoint restore joins the governed network and carries the authenticated
  principal's tenant label; without `AGENT_EGRESS_NETWORK` it refuses instead of
  deriving a container on the default bridge. The restore spec also carries the
  proxy environment in `proxy` mode, so a restored sandbox stays on the audited
  path instead of silently going direct.
* Enforcement is read from the environment at the single construction point of a
  run (`app::agent_config`), not from the request body: `AGENT_EGRESS_ENFORCEMENT`,
  `AGENT_EGRESS_NETWORK`, `AGENT_EGRESS_PROXY_URL`. Boot publishes the last two
  only after the network and proxy are healthy, so a run cannot claim `network`
  with an unset network name.
* The control plane's `/internal/egress/*` surface must not be published by the
  reverse proxy. It is token-guarded, but it belongs on the control network only.
