#!/usr/bin/env bash
#
# Line-coverage gate for the agent workspace.
#
# Runs the whole test suite under `cargo-llvm-cov`, then fails unless **every
# included source line** is executed at least once. "Included" excludes the two
# categories that carry no statement of their own: closing-delimiter artifacts
# emitted by llvm, and lines that opted out with `// LCOV_EXCL_LINE`. Both are
# printed separately and the summary reports the measured percentage next to the
# included-line percentage the gate actually enforces.
#
# Coverage is measured on the lcov `DA:` records (one record per real source
# line) instead of `llvm-cov --fail-under-lines`, because the summary metric
# also counts duplicate records from macro / generic instantiations. Those
# duplicates cannot be mapped back to a source line and would report a file as
# uncovered while its annotated source is fully green; they are printed
# separately for transparency.
#
# Exclusions (see `docs/coverage.md`): the FFI binding generator, the cloud
# process entrypoint, and the Postgres/pgvector backends (they need a live
# database; their `DATABASE_URL`-gated suite is green against a real server, and
# `just pg-test` / the `postgres` CI job run it). Outside the IGNORE list a single
# statement may opt out with `// LCOV_EXCL_LINE` (printed as a separate count),
# and closing-delimiter-only lines are ignored as artifacts.
#
# Usage: ./scripts/coverage.sh [extra cargo-llvm-cov args]

set -euo pipefail
cd "$(dirname "$0")/.."

# `store/governance.rs` is the Postgres half of the governance store: same
# category as `store/pg.rs` (DB-gated integration behaviour, exercised only when
# DATABASE_URL is set), so it is excluded from the line gate for the same reason.
# `aria-agent-egress-proxy/src/main.rs` is the bin entry point (same category as
# `aria-agent-cloud/src/main.rs`).
#
# Two files left this list:
#   * `tools/web.rs` — its only unexercised line was `host_of` with an empty
#     host, which needs no network at all.
#   * `sandbox/egress_net.rs` — the Docker calls are covered by a scripted
#     Engine API over loopback (`Engine` in its test module), so `ensure`,
#     `probe`, `health`, `resolve_peer` and `control_networks` run against the
#     real request/response path without a daemon.
# `tools/terminal.rs` stays: the `pty` feature (off by default) needs a real
# terminal device.
IGNORE='crates/aria-agent-ffigen/|crates/aria-agent-cloud/src/main\.rs|crates/aria-agent-cloud/src/store/pg\.rs|crates/aria-agent-cloud/src/context/pg\.rs|crates/aria-agent-cloud/src/store/pg_orchestration\.rs|crates/aria-agent-cloud/src/store/pg_rsi\.rs|crates/aria-agent-cloud/src/store/governance\.rs|crates/aria-agent-cloud/src/pg_testkit\.rs|crates/aria-agent-tools/src/terminal\.rs|crates/aria-agent-dsh/src/transport\.rs|crates/aria-agent-egress-proxy/src/main\.rs'

# Hermetic environment: the sandbox socket probes and the Docker client
# constructor are environment-dependent. Give both a stable, local target so
# the gate does not rely on the runner's XDG_RUNTIME_DIR or a real daemon.
COV_TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$COV_TMP_DIR"' EXIT
export XDG_RUNTIME_DIR="$COV_TMP_DIR/xdg-runtime"
mkdir -p "$XDG_RUNTIME_DIR"
chmod 700 "$XDG_RUNTIME_DIR"
export DOCKER_HOST="unix://$COV_TMP_DIR/docker.sock"
touch "$COV_TMP_DIR/docker.sock"

echo "==> running the workspace under coverage"
cargo llvm-cov --workspace --lcov --output-path lcov.info \
    --ignore-filename-regex "$IGNORE" "$@"

echo "==> checking line coverage"
IGNORE="$IGNORE" python3 - <<'PY'
import os
import re
import sys

ignore = re.compile(os.environ["IGNORE"])
root = os.getcwd()
covered = missed = excluded = 0
gaps = []
current = None

# A single line may opt out of the gate with a trailing `// LCOV_EXCL_LINE`
# marker. This exists for statements that can only run when the OS refuses an
# operation we must still report (a socket write to a peer that vanished, an
# atomic rename that fails): keeping the error branch is right, faking the
# failure hermetically is not. The marker must sit on the statement itself, and
# the surrounding lines stay in the gate.
MARKER = "LCOV_EXCL_LINE"

# llvm emits a region for the closing delimiter of some blocks (`}`, `});`)
# that is never entered even when the block body runs. A line made only of
# closing delimiters carries no statement of its own, so it cannot be a real
# coverage gap: any unexecuted statement inside the block is still reported.
CLOSERS = re.compile(r"^[\s\}\)\]\;,]+$")
artifacts = 0

with open("lcov.info", encoding="utf8", errors="replace") as handle:
    for raw in handle:
        line = raw.strip()
        if line.startswith("SF:"):
            current = line[3:]
            current = current if ignore.search(current) is None else None
        elif current and line.startswith("DA:"):
            number, hits = line[3:].split(",")[:2]
            if int(hits) == 0:
                missed += 1
                gaps.append((current, int(number)))
            else:
                covered += 1

for path, number in gaps:
    text = ""
    try:
        with open(path, encoding="utf8", errors="replace") as src:
            for i, row in enumerate(src, 1):
                if i == number:
                    text = row.rstrip()
                    break
    except OSError:
        pass
    if MARKER in text:
        excluded += 1
        continue
    if CLOSERS.match(text):
        artifacts += 1
        continue
    print(f"{os.path.relpath(path, root)}:{number}: {text.strip()[:120]}")

# The gate is over *included* lines: measured lines minus the two categories that
# carry no statement of their own (closing-delimiter artifacts) or that opted out
# explicitly (`LCOV_EXCL_LINE`). Reporting the raw percentage next to the gate
# percentage keeps both honest: the first shows what llvm measured, the second
# is the metric the gate and the CI job name actually promise.
total = covered + missed
included = total - excluded - artifacts
raw = 100.0 * covered / total if total else 100.0
pct = 100.0 * covered / included if included else 100.0
print(f"\nlines measured: {covered}/{total} ({raw:.2f}%)")
if excluded:
    print(f"excluded by `{MARKER}`: {excluded}")
if artifacts:
    print(f"closing-delimiter artifacts: {artifacts} (no statement; not counted)")
print(f"lines included: {covered}/{included} ({pct:.2f}%)")

if missed > excluded + artifacts:
    print(f"FAIL: {missed - excluded - artifacts} uncovered line(s).")
    sys.exit(1)

print("OK: every included line is covered (or explicitly excluded).")
PY
