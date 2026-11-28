# ADR-0027: the dsh session runs inside a sandbox container

## Status

Accepted.

## Context

ADR-0019 made DeepSeek Harness (`dsh --profile sdk`) the default turn runtime and
put it on the host as a child process over JSON-RPC on stdio. That is honest for
the *model* path but false for the *tool* path: dsh executes every tool call
itself, so with `harness = dsh` the aria `Sandbox`, the `ToolExecutionPipeline`
and the registered handlers never run. The isolation of a dsh session was dsh's
own permission preset (`workspace-write`) — a **policy check inside the process**,
not a filesystem or kernel boundary.

`docs/sandbox-dsh-feasibility.md` evaluated containerising the harness and listed
six blockers in the `Sandbox` seam: no stdin attach, no streaming output, no
command-carrying spawn, no workspace persistence, `timeout_ms` never enforced, and
no dsh binary in the sandbox image. It also rejected two shortcuts: `cube` cannot
honour a governed network and `browser` is not a general execution backend.

## Decision

### 1. A dsh session is a container session (form F2)

`Sandbox::spawn` creates a keep-alive container; the harness runs **inside it** as
an attached `exec` (`Sandbox::exec_attached`, `attach_stdin/stdout/stderr`). The
container owns the process lifetime: "stop the harness" is `destroy`.

Rejected alternatives:

* `attach_container` with dsh as pid 1 (form F3): bollard decodes that stream with
  `NewlineLogOutputDecoder`, which is wrong for a non-tty, 8-byte-framed stream;
  `start_exec`'s duplex path is the one already proven against the Engine stub.
* one prompt per container (form F1): the `initialize` handshake is bounded at
  30 s and the session history would be reloaded on every turn.

### 2. Session state is a per-session **named volume**

The cloud runs Docker-out-of-Docker: `docker-compose.yml` mounts only
`/var/run/docker.sock`, so a bind mount's source is resolved on the daemon's host
and a control-plane path would silently mount nothing. The workspace, `DSH_HOME`
and the links dsh needs therefore live on a volume:

```
aria-dsh-<tenant>-<session>  →  /aria/session
                                  ├── dsh   (DSH_HOME)
                                  └── cwd   (agent workspace, .aria/skills)
```

The volume outlives the container on purpose: when the pool evicts a runtime (or
the process restarts), the next `session/prompt` gets a fresh container on the
same volume and dsh resumes its history. Volumes are pruned by name prefix
(`docker volume ls --filter name=aria-dsh-`). `DSH_SANDBOX_MOUNT=bind` exists for
deployments where the control plane shares the daemon's filesystem; it has two
requirements that a named volume does not, both found by the live check: the
directory must be writable by the harness user (Docker populates a volume from the
image, already owned by that user, while a bind mount is not populated at all), and
the image must not declare its `WORKDIR` inside the mount — runc creates a missing
working directory as root, so `/aria/session/cwd` would appear root-owned in the
bind source and the capability-less harness could not write it. `prepare()` creates
`DSH_HOME` and the workspace as the container user, which is what makes either
mount mode work.

Paths inside the container are **container paths**. `DshConfig::with_sandbox`
re-derives `home`/`cwd` and the exec environment uses the image's `PATH`, never
the host's.

### 3. The skill catalog is uploaded, not symlinked

`shared_skill_catalog()` is a control-plane directory; a container cannot see it.
At spawn the catalog is packed as a tar whose top level is `skills/` and unpacked
into `<cwd>/.aria` (`Sandbox::copy_in`). An empty/missing catalog uploads nothing,
because an empty directory would make `skill_list` look configured when it is not.

### 4. Fail closed, on by default

`DSH_SANDBOX` defaults to `1`. A missing image, an unreachable workspace, a
refused upload, an unattachable harness or a failed handshake **fails the turn**
and destroys the container; there is no fallback to a host process, because a
silent fallback turns "sandboxed" into a claim. `DSH_SANDBOX=0` is the explicit,
logged escape hatch for local debugging.

Boot preflight (before the first turn): the image must already be present
(`Sandbox::has_image`). This repository has no registry client, so the operator
gets the build command instead of a 500 on turn one.

### 5. The thinking filter needs a reachable address

dsh cannot replay an Anthropic `redacted_thinking` block, so `DSH_THINKING_FILTER`
(default `enable`) fronts the gateway with a stripping proxy. Inside a container
`127.0.0.1` is the container itself, so:

* `DSH_THINKING_FILTER_BIND` (default `127.0.0.1:0`) is where the proxy listens;
* `DSH_THINKING_FILTER_URL` is what dsh is told to use.

With a sandboxed harness, `enable` plus a missing or loopback URL **fails boot**.
The advertised host is appended to `NO_PROXY` so the filter is not routed through
the egress proxy.

### 6. Hardening is explicit and scoped to dsh

`SandboxHardening` (`user`, `cap_drop`, `read_only_rootfs`, `no_new_privileges`)
defaults to **off** so the shell/jobs families keep their behaviour; the dsh
sandbox turns on `cap_drop=ALL` + `no-new-privileges` and runs as the image's
non-root uid. `read_only_rootfs` stays off by default: dsh needs `/tmp` and its
home volume.

### 7. `timeout_ms` is enforced where the budget is known

`SandboxShell` wraps `spawn` + `exec` in a `timeout(ExecSpec::timeout_ms)` and
always destroys the container afterwards, so an overrunning command cannot leak a
session. The sandboxed transport additionally bounds idle reads with the request
timeout, because a container can die without closing a pipe.

## Consequences

* A dsh session is now a container: `DSH_POOL_MAX` (default 8) means up to 8
  containers per tenant, each with its own volume. The image is heavier than a
  shell image, so the harness gets its own budget (`DSH_SANDBOX_CPUS/MEMORY/PIDS`,
  default 1 CPU / 1 GiB / 512 pids).
* **Credentials:** `DEEPSEEK_API_KEY` is passed in the container environment, so
  `docker inspect` can see it (it is visible to the harness either way). Moving to
  the `${credential}` placeholder with egress-side substitution is a follow-up and
  is recorded as a risk rather than silently accepted.
* **Egress:** model traffic still goes through the governed proxy
  (ADR-0024/0025); the container joins the internal network and carries the
  `aria.tenant_id` / `aria.session_id` labels.
* **Not yet verified at runtime** (recorded in the feasibility document): whether
  dsh honours `HTTPS_PROXY` including `CONNECT`, whether `cap_drop=ALL` breaks
  dsh's own sandbox, and the real memory envelope. `just dsh-sandbox-e2e` is the
  gate that answers them against a real image.
* The container is **not** metered yet: `QuotaGuardSandbox` wraps the tool
  sandbox, not the harness backend. Host-mode dsh was not metered either; this is
  a known gap, not a regression.
* `harness = native` is unchanged: its tools still run through the `Sandbox`
  seam, with the same one-container-per-command shell behaviour (path B in the
  feasibility document is a separate milestone).

## References

* `docs/adr/0019-dsh-runtime.md` — the harness as the turn runtime.
* `docs/adr/0024-network-enforcement-sidecar.md`, `0025-governed-egress-single-boundary.md`
* `docs/adr/0022-sandbox-checkpoint-and-metering.md`
* `docs/sandbox-dsh-feasibility.md` — the gap analysis this ADR closes.
