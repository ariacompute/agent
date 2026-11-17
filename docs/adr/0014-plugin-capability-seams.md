# ADR-0014 — Plugin registry and capability seams

Status: Accepted.

## Context

The runtime grew by accretion: the run loop, tools, storage and sandbox were each
special-cased, and adding a capability meant editing the loop and the cloud wiring.
We want a kernel where every capability is a named contract, providers are
pluggable, and a deployment that is missing a provider **cannot** start half-wired.

Two designs were considered:

1. A Cordis-style runtime container: `dyn Any` service locator, hot install /
   uninstall, lifecycle callbacks.
2. A **builder + freeze** registry: providers are registered during assembly, the
   registry is validated and frozen, and the agent resolves every seam once.

## Decision

* Three roles (`SeamRole`):
  * `Core` — an authoritative capability owned by the kernel (run loop, session
    projection). Exactly one provider per `core` key.
  * `Seam` — a replaceable contract (`llm`, `sandbox`, `storage`, `approvals`, …)
    with at least one provider.
  * `Bundle` — a plugin that only contributes tools/handlers on top of seams.
* A `PluginManifest` declares `id`, `role`, `provides: Vec<SeamKey>` and
  `requires: Vec<SeamKey>`. `SeamKey` is a newtype over `&'static str`; provider
  names are static strings, so lookups are key comparisons rather than hashing on
  the hot path.
* `Registry` is mutable during assembly and `freeze()` yields an immutable handle.
  Assembly order: collect manifests → reject duplicate `id` → validate one
  provider per `core` key → topo-sort by `requires` → cycle detection → install in
  order → verify every consumed key resolved. **Any** failure returns
  `PluginError` and the caller aborts boot.
* `Agent::with_handlers` resolves all seams once into struct fields. Nothing in
  `run_event_stream` touches the registry, so a tool call costs zero lookups.

## Rejected alternative

Runtime DI was rejected: Rust has no prototype chain or interception-based
context, so hot install needs `dyn Any` + read/write locks + lifecycle callbacks.
This repo has no local UI and no plugin marketplace, so the complexity buys
nothing while making failure modes implicit. Fail-closed assembly at boot is both
simpler and stricter.

## Consequences

* A misconfigured deployment fails at startup with a complete dependency chain
  (`error!` once, no warning storm) instead of at the first tool call.
* Providers can be swapped per deployment (pg vs sqlite, docker vs kata, memory
  vs HTTP) without touching the loop.
* Extension cost is one trait impl plus a manifest — no core edit.
* Tool handlers see seams through context, so `aria-agent-core` keeps not
  depending on any store or sandbox implementation.
