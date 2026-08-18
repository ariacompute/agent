#!/usr/bin/env bash
#
# One-shot bootstrap for a local CubeSandbox (E2B-compatible sandbox service).
#
# Requirements:
#   - x86_64 Linux with KVM (/dev/kvm) — CubeSandbox manages KVM MicroVMs
#   - docker + curl (for the official online-install script)
#   - cubemastercli (installed by online-install)
#
# On success it writes E2B_API_URL / E2B_API_KEY / CUBE_TEMPLATE_ID (and
# ARIA_WORKSPACE_ROOT) into .env at the repo root, ready for `pnpm dsh web`.
#
# Network note: if GitHub/registry access fails, export https_proxy first:
#   export https_proxy=http://127.0.0.1:7897
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
ENV_FILE="$REPO_ROOT/.env"
CUBE_REPO="https://github.com/TencentCloud/CubeSandbox/raw/master"
INSTALL_SCRIPT="$CUBE_REPO/deploy/one-click/online-install.sh"
IMAGE_DEFAULT="cube-sandbox-int.tencentcloudcr.com/cube-sandbox/sandbox-code:latest"
API_KEY="${E2B_API_KEY:-e2b_000000}"
WORKSPACE_ROOT="${ARIA_WORKSPACE_ROOT:-$HOME/.ariacompute/agent/workspaces}"

echo "==> aria-agent CubeSandbox bootstrap"
echo "    repo: $REPO_ROOT"

if [ ! -e /dev/kvm ]; then
  echo "ERROR: /dev/kvm not found. CubeSandbox requires KVM-enabled x86_64 Linux." >&2
  echo "       On a VM enable nested virtualization, or run on a KVM bare metal host." >&2
  exit 1
fi

# 1) Install CubeSandbox control plane (CubeAPI :3000) via official one-click script.
if ! command -v cubemastercli >/dev/null 2>&1; then
  echo "==> cubemastercli not found; running official online-install (needs root/docker)..."
  echo "    from: $INSTALL_SCRIPT"
  if [ "$(id -u)" -ne 0 ] && ! sudo -n true 2>/dev/null; then
    echo "ERROR: online-install needs root. Run as root or with passwordless sudo." >&2
    exit 1
  fi
  curl -fsSL "$INSTALL_SCRIPT" | CUBE_PVM_ENABLE=1 bash
fi

command -v cubemastercli >/dev/null 2>&1 || {
  echo "ERROR: cubemastercli still missing after install." >&2
  exit 1
}

# 2) Health check: CubeAPI REST should answer on :3000.
if ! curl -fsS "${E2B_API_URL:-http://127.0.0.1:3000}/health" >/dev/null 2>&1; then
  echo "WARN: CubeAPI not answering at ${E2B_API_URL:-http://127.0.0.1:3000} yet." >&2
  echo "      If the install finished, wait a few seconds and re-run this script." >&2
fi

# 3) Create the code sandbox template unless CUBE_TEMPLATE_ID is already set.
if [ -z "${CUBE_TEMPLATE_ID:-}" ]; then
  echo "==> creating sandbox-code template (this pulls the image; can take a while)..."
  IMAGE="${CUBE_TEMPLATE_IMAGE:-$IMAGE_DEFAULT}"
  JOB_OUTPUT="$(cubemastercli tpl create-from-image \
    --image "$IMAGE" \
    --writable-layer-size 1G \
    --expose-port 49983 \
    --expose-port 49999 \
    --probe 49999)"
  echo "$JOB_OUTPUT"
  JOB_ID="$(echo "$JOB_OUTPUT" | grep -oE 'job[-_ ]?[a-zA-Z0-9_-]+' | head -1 || true)"
  if [ -n "$JOB_ID" ]; then
    echo "==> waiting for template build job $JOB_ID ..."
    cubemastercli tpl watch --job-id "$JOB_ID"
  fi
  echo "==> template created. Copy CUBE_TEMPLATE_ID from the output above, then run:"
  echo "    export CUBE_TEMPLATE_ID=<id>"
  echo "    $0"
  exit 0
fi

# 4) Write .env for the aria-sandbox plugin.
echo "==> writing $ENV_FILE"
cat > "$ENV_FILE" <<EOF
E2B_API_URL=http://127.0.0.1:3000
E2B_API_KEY=$API_KEY
CUBE_TEMPLATE_ID=$CUBE_TEMPLATE_ID
ARIA_WORKSPACE_ROOT=$WORKSPACE_ROOT
ARIA_WORKSPACE_SYNC_AFTER_EXEC=false
EOF

echo "==> done. Start the agent with:"
echo "    pnpm dsh web --patch $REPO_ROOT/dsh/cordis.patch.yml"
