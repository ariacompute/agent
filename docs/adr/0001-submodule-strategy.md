# ADR 0001: codex submodule strategy

## Status
Accepted.

## Context
We build on OpenAI's `codex` (Rust harness, SDK, sandboxing, memories). We need
its code available in-tree without fork-maintenance overhead.

## Decision
* Add `https://github.com/openai/codex.git` as a git submodule at `codex/`.
* Pin to a stable release tag (not `main`) for reproducibility; patch via the
  submodule's existing `patches/` directory when required.
* Do **not** add `codex/codex-rs` to our Cargo workspace as a member. Our crates
  reference codex crate shapes by design and may add precise path dependencies
  to individual `codex-rs/*` crates when deeper integration is needed. This
  avoids workspace-member conflicts and keeps our build self-contained.

## Consequences
* `git submodule update --init --depth 1 codex` is required before building.
* Our workspace builds independently of codex's Bazel/Nix toolchain.
