# AGENTS.md — contribution & architecture conventions

This repo is a Rust workspace that wraps the OpenAI **codex** harness and ships a
cloud API plus native (Swift/Kotlin) SDKs.

## Workspace layout

* `crates/aria-agent-memo`, `aria-agent-sandbox`, `aria-agent-core`, `ariacompute-agent`,
  `aria-agent-cloud`, `aria-agent-reef`, `aria-agent-ffigen`
* `bindings/swift` (`Package.swift` SwiftPM + `AriaAgent.podspec` CocoaPods),
  `bindings/kotlin` (Android `build.gradle.kts` with vanniktech Maven publish).
  Generated Swift/Kotlin *sources* are committed and must not be edited by hand —
  regenerate via `just ffi`; the podspec and Gradle publishing config are hand-maintained.
* `codex/` is a **git submodule** (do not add it to the Cargo workspace).

## Rules

1. **memo is the only context store.** Never persist conversational/long-term
   context in Postgres. Postgres (`aria-agent-cloud`) holds metadata only
   (`agents`, `runs`). See `docs/adr/0004-memo-storage.md`. `aria-agent-memo` is a
   sled-backed `MemoStore` (`memorize` / `recall` / `compact` / `get_by_key`)
   with `ContextFragment` (`key` + `FragmentKind` + optional dense `embedding`)
   and local hashing-trick + cosine semantic recall; its unit tests cover
   normal + abnormal paths (`cargo test -p aria-agent-memo`).
2. **FFI is stable.** Only change `ariacompute-agent`'s exported surface deliberately.
   After any change run `just ffi` and commit the regenerated bindings. See
   `docs/adr/0002-ffi-boundary.md`.
3. **Sandbox is pluggable.** Add providers by implementing the `Sandbox` trait
   and registering them in `from_provider`. Docker is the default. See
   `docs/adr/0003-sandbox-providers.md`.
4. **Submodule discipline.** Keep `codex` out of the workspace member list;
   reference codex crate shapes by design. Deep compile-integration of codex's
   `sandboxing`/`memories` crates is prescribed in
   `docs/adr/0005-codex-integration.md` (currently blocked by a broken upstream
   dependency; our `aria-agent-sandbox`/`aria-agent-memo` are the self-contained
   integration seam). See `docs/adr/0001-submodule-strategy.md`.
5. **Secrets/logging.** Use `tracing`. Never log the OpenAI key or raw user
   content.
6. **Tests.** `cargo test --workspace` must pass. Cross-crate behavior belongs
   in `tests/`.
7. **Cloud auth + streaming.** `aria-agent-cloud` is gated by `AGENT_CLOUD_API_KEY`
   (send `Authorization: Bearer <key>` or `ApiKey <key>`; open when unset) and
   serves token streaming at `POST /v1/runs/stream` (SSE, terminated by
   `[DONE]`).
8. **Reef stores are separate from context/metadata.** `aria-agent-reef` logs every
   turn (`RecordStore`) and binds feedback (`FeedbackStore`) in a **local sled
   DB**, and versions winning harnesses in a **`.reef/` Git repo** — never in
   memo (context store) nor Postgres (metadata only). The served harness is a
   shared `ActiveHarness` hot-swapped atomically (no restart). See
   `docs/adr/0006-reef-self-improvement.md`. Cloud routes: `POST /reef/report`,
   `POST /reef/evolve`, `GET /reef/versions`.

9. **Releases & publishing.** On `release: created`, `.github/workflows/release.yml`
   builds/tests/packages the `aria-agent-cloud` binary and `ariacompute-agent` FFI cdylib across
   linux-x86_64 / windows-x86_64 / linux-arm64 / macos and uploads them to the GitHub
   Release (`secrets.ARIACOMPUTE_TOKEN`). Its `publish-packages` job is fail-pass
   (`continue-on-error: true`) and only stubs language-package publishing so it never
   blocks the CLI/FFI assets. Real publishes run in separate workflows:
   `publish-cargo.yml` (crates.io: `aria-agent-memo` → `aria-agent-sandbox` → `aria-agent-core` →
   `ariacompute-agent`, topological order, `secrets.CARGO_REGISTRY_TOKEN`) and `publish-maven.yml`
   (Maven Central `com.ariacompute:agent` via vanniktech, `secrets.SONATYPE_*` +
   `secrets.GPG_*`). `aria-agent-cloud` (CLI) and `aria-agent-ffigen` (`publish = false`) are NOT
   published to crates.io.

## Common commands

* `just build` — build all crates
* `just test` — run tests
* `just reef-test` — run `aria-agent-reef` + integration tests (no API key / Postgres)
* `just cov` — `cargo tarpaulin` coverage (if available)
* `just ffi` — regenerate Swift/Kotlin bindings
* `just cloud` — run the cloud service (`aria-agent serve` is the default subcommand;
  it reads `~/.ariacompute/agent-cli.yml` and exports `ARIA_AGENT_FFI_LIB` to `~/.ariacompute/lib`)
* `aria-agent setup` — interactively choose the Releases source (github default
  or gitee) and write `upgrade_url` to `~/.ariacompute/agent-cli.yml`
  (override home via `ARIA_COMPUTE_HOME`). Non-interactive runs default to github.
* `aria-agent setup --status` — show CLI config status (path, `upgrade_url`, lib dir).
* `aria-agent setup --clear` — remove the CLI config file.
* `aria-agent upgrade [version] [--url <url>]` — self-update from GitHub/Gitee
  Releases: downloads `aria-agent` binary + `libaria-agent_ffi` cdylib
  (`~/.ariacompute/lib`, set `ARIA_AGENT_FFI_LIB` if needed). Mirrors
  `aria-router upgrade`; errors if `upgrade_url` is unset.
* `aria-agent --version` / `aria-agent version` — print version
* `just fmt` / `just lint` — formatting and clippy
* Release & publish: see `.github/workflows/release.yml` (assets),
  `publish-cargo.yml` (crates.io), `publish-maven.yml` (Maven Central); Swift also
  via CocoaPods `bindings/swift/AriaAgent.podspec`.
