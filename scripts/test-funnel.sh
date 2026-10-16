#!/usr/bin/env bash
#
# test-funnel.sh — smoke test for the aria-agent Cloud API THROUGH the public
# Tailscale Funnel endpoint (https://ariacompute-funnel.saury-viper.ts.net).
#
# This is a thin wrapper around the same checks as scripts/run-api-tests.sh, but:
#   * defaults BASE_URL to the Funnel HTTPS endpoint (no port needed — Funnel
#     serves on :443 with a valid *.ts.net certificate)
#   * defaults EVENTS_TEST to 1 (a deployed/gateway-configured service supports
#     the SSE /events/stream endpoint)
#   * adds --insecure (-k) to relax TLS verification when diagnosing a
#     certificate/handshake problem (off by default — Funnel certs are valid)
#
# The agent service itself is expected to be up and reachable behind the Funnel
# (see ../docker-compose.yml + the aria-tailscale Funnel config). This script
# only probes the public URL; it does NOT start/stop anything.
#
# It loads the SAME env file docker compose uses (for AGENT_CLOUD_API_KEY):
#   * $SCRIPT_DIR/../.env            (preferred — the file compose actually reads)
#   * $SCRIPT_DIR/../.env.example    (fallback — the canonical template)
# BASE_URL / EVENTS_TEST / INSECURE are NOT read from that file.
#
# Usage:
#   bash scripts/test-funnel.sh                       # uses the default Funnel URL
#   bash scripts/test-funnel.sh --insecure            # relax TLS (diagnose certs)
#   BASE_URL=https://other.host bash scripts/test-funnel.sh
#   bash scripts/test-funnel.sh -u https://host -e    # -e/--events-test is default-on
#   (also: -t/--ready-timeout SECONDS; -k/--insecure; -h/--help)
#
# Exit code = number of failed checks (0 == all passed).

set -uo pipefail

# ----------------------------------------------------------------- args parse
ARG_BASE_URL=""
ARG_EVENTS_TEST=""
ARG_INSECURE=""
while [ $# -gt 0 ]; do
  case "$1" in
    -u|--base-url)    ARG_BASE_URL="${2:-}"; shift 2;;
    --base-url=*)     ARG_BASE_URL="${1#*=}"; shift;;
    -e|--events-test) ARG_EVENTS_TEST=1; shift;;
    --events-test=*)  ARG_EVENTS_TEST="${1#*=}"; shift;;
    -k|--insecure)    ARG_INSECURE=1; shift;;
    --insecure=*)     ARG_INSECURE="${1#*=}"; shift;;
    -t|--ready-timeout) READY_TIMEOUT="${2:-}"; shift 2;;
    --ready-timeout=*)  READY_TIMEOUT="${1#*=}"; shift;;
    -h|--help)        grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'; exit 0;;
    *) echo "ERROR: unknown argument: $1" >&2; exit 2;;
  esac
done

# ---------------------------------------------------------------- config / env
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ENV_FILE="$SCRIPT_DIR/../.env"
[ -f "$ENV_FILE" ] || ENV_FILE="$SCRIPT_DIR/../.env.example"
if [ -f "$ENV_FILE" ]; then
  # Load compose's env (for AGENT_CLOUD_API_KEY etc.) WITHOUT clobbering variables
  # already exported in the shell, and WITHOUT importing BASE_URL / EVENTS_TEST /
  # INSECURE (those are configured via the command line or environment).
  while IFS='=' read -r k v; do
    k="${k#"${k%%[![:space:]]*}"}"; k="${k%"${k##*[![:space:]]}"}"
    [ -z "$k" ] && continue
    case "$k" in
      \#*|BASE_URL|EVENTS_TEST|INSECURE) continue;;
    esac
    v="${v#"${v%%[![:space:]]*}"}"; v="${v%"${v##*[![:space:]]}"}"
    [ -z "${!k+x}" ] && export "$k=$v"
  done < "$ENV_FILE"
fi

# Precedence: CLI arg > exported env var > default.
# Defaults: public Funnel HTTPS endpoint, events test ON (deployed service).
DEFAULT_BASE_URL="https://ariacompute-funnel.saury-viper.ts.net"
BASE_URL="${ARG_BASE_URL:-${BASE_URL:-$DEFAULT_BASE_URL}}"
EVENTS_TEST="${ARG_EVENTS_TEST:-${EVENTS_TEST:-1}}"
INSECURE="${ARG_INSECURE:-${INSECURE:-}}"
ADMIN_KEY="${AGENT_CLOUD_API_KEY:-}"   # reused as the Admin Bearer
READY_TIMEOUT="${READY_TIMEOUT:-60}"

command -v curl >/dev/null 2>&1 || { echo "ERROR: curl is required." >&2; exit 1; }
if ! command -v jq >/dev/null 2>&1; then
  echo "ERROR: jq is required (used to extract ids and assert JSON fields)." >&2
  exit 1
fi

# ----------------------------------------------------------------- temp files
BODY_FILE="$(mktemp)"
SSE_FILE="$(mktemp)"
cleanup() { rm -f "$BODY_FILE" "$SSE_FILE"; }
trap cleanup EXIT

# TLS options: when --insecure is set, append -k so a bad/self-signed cert (or a
# not-yet-provisioned Funnel cert) does not mask a connectivity problem.
TLS_OPTS=()
[ -n "$INSECURE" ] && TLS_OPTS+=(-k)

# --------------------------------------------------------------- http helpers
RESP=""   # response body of the last req
CODE=""   # http status code of the last req

# req METHOD PATH [BODY]
req() {
  local method="$1" path="$2" body="${3:-}"
  local -a args=(
    -sS "${TLS_OPTS[@]}"
    -X "$method"
    -H "OpenAI-Beta: agents=v1"
    -o "$BODY_FILE" -w '%{http_code}'
  )
  [ -n "$ADMIN_KEY" ] && args+=(-H "Authorization: Bearer $ADMIN_KEY")
  [ -n "$body" ] && args+=(-H "Content-Type: application/json" -d "$body")
  CODE="$(curl "${args[@]}" "$BASE_URL$path")"
  RESP="$(cat "$BODY_FILE")"
}

# check METHOD PATH EXPECTED [JQ_FIELD] [BODY]
# Passes when the status equals EXPECTED and, if JQ_FIELD is given, that field
# is present (non-empty) in the JSON response.
check() {
  local method="$1" path="$2" expected="$3" field="${4:-}" body="${5:-}"
  req "$method" "$path" "$body"
  local ok=1
  if [ "$CODE" != "$expected" ]; then
    ok=0
  elif [ -n "$field" ]; then
    local val
    val="$(printf '%s' "$RESP" | jq -r "$field // empty" 2>/dev/null || true)"
    [ -z "$val" ] && ok=0
  fi
  TOTAL=$((TOTAL + 1))
  if [ "$ok" -eq 1 ]; then
    PASS=$((PASS + 1))
    printf "  PASS  %-5s %s\n" "$method" "$path"
  else
    FAIL=$((FAIL + 1))
    printf "  FAIL  %-5s %s (got HTTP %s)\n" "$method" "$path" "$CODE"
    [ -n "$field" ] && printf "        expected field %s present in: %s\n" "$field" "${RESP:0:200}"
  fi
}

# skip PATH REASON
skip() {
  local path="$1" reason="$2"
  TOTAL=$((TOTAL + 1))
  SKIP=$((SKIP + 1))
  printf "  SKIP  %s (%s)\n" "$path" "$reason"
}

jqid() { printf '%s' "$RESP" | jq -r "$1 // empty" 2>/dev/null || true; }

# -------------------------------------------------------------- readiness probe
echo "==> Probing $BASE_URL/v1/agents (admin Bearer ${ADMIN_KEY:+set}${ADMIN_KEY:-<open>})${INSECURE:+ [insecure TLS]}"
i=0
while [ "$i" -lt "$READY_TIMEOUT" ]; do
  req GET /v1/agents
  [ "$CODE" = "200" ] && break
  sleep 1
  i=$((i + 1))
done
if [ "$CODE" != "200" ]; then
  echo "ERROR: service not ready at $BASE_URL after ${READY_TIMEOUT}s (last HTTP $CODE)." >&2
  if [ -z "$INSECURE" ]; then
    echo "  If this is a TLS/certificate error, retry with --insecure to confirm." >&2
  fi
  echo "  Verify the Funnel node is connected and serving:" >&2
  echo "    tailscale status            # node ariacompute-funnel must be online" >&2
  echo "    # and the Funnel cert/endpoint is live for the *.ts.net name" >&2
  exit 1
fi
echo "==> Service is up."

# ------------------------------------------------------------------- counters
TOTAL=0
PASS=0
FAIL=0
SKIP=0

# ===================================================================== AGENTS
echo
echo "== Agents"
check POST /v1/agents 200 '.id' \
  '{"name":"smoke-agent","model":"gpt-4o-mini","instructions":"be brief"}'
AGENT_ID="$(jqid '.id')"
check GET /v1/agents 200 '.data'
if [ -n "$AGENT_ID" ]; then
  check GET "/v1/agents/$AGENT_ID" 200 '.id'
  check POST "/v1/agents/$AGENT_ID" 200 '.id' '{"instructions":"be very brief"}'
else
  skip "/v1/agents/:id" "no agent id captured"
fi

# =================================================================== SESSIONS
echo
echo "== Sessions"
if [ -n "$AGENT_ID" ]; then
  check POST /v1/agents/sessions 200 '.id' "{\"agent_id\":\"$AGENT_ID\"}"
  SESSION_ID="$(jqid '.id')"
else
  # Fallback: try to reuse an existing agent (list) if creation above failed.
  req GET /v1/agents
  AGENT_ID="$(jqid '.data[0].id')"
  check POST /v1/agents/sessions 200 '.id' "{\"agent_id\":\"$AGENT_ID\"}"
  SESSION_ID="$(jqid '.id')"
fi
check GET /v1/agents/sessions 200 '.data'
if [ -n "$SESSION_ID" ]; then
  check GET "/v1/agents/sessions/$SESSION_ID" 200 '.id'
  check POST "/v1/agents/sessions/$SESSION_ID" 200 '.id' '{"instructions":"session-only note"}'
else
  skip "/v1/agents/sessions/:id" "no session id captured"
fi

# ============================================================== READ-ONLY LISTS
echo
echo "== Items / Turns (read-only)"
if [ -n "$SESSION_ID" ]; then
  check GET "/v1/agents/sessions/$SESSION_ID/items" 200 '.data'
  check GET "/v1/agents/sessions/$SESSION_ID/turns" 200 '.data'
else
  skip "items/turns" "no session id captured"
fi

# ==================================================================== MEMORY
echo
echo "== Memory"
if [ -n "$SESSION_ID" ]; then
  check POST "/v1/agents/sessions/$SESSION_ID/memory" 200 '.id' \
    '{"key":"greeting","value":"hello world","kind":"long_term"}'
  MEM_A="$(jqid '.id')"
  check POST "/v1/agents/sessions/$SESSION_ID/memory" 200 '.id' \
    '{"value":"second fragment","kind":"message"}'
  MEM_B="$(jqid '.id')"
  check GET "/v1/agents/sessions/$SESSION_ID/memory" 200 '.data'
  check GET "/v1/agents/sessions/$SESSION_ID/memory/greeting" 200 '.id'
  # relations: create -> list -> graph -> delete
  if [ -n "$MEM_A" ] && [ -n "$MEM_B" ]; then
    check POST "/v1/agents/sessions/$SESSION_ID/memory/relations" 200 '.from_id' \
      "{\"from_id\":\"$MEM_A\",\"to_id\":\"$MEM_B\",\"kind\":\"temporal\",\"score\":0.9}"
    check GET "/v1/agents/sessions/$SESSION_ID/memory/relations" 200 '.data'
    # Query strings belong in the URL (the 5th `check` argument is a request
    # BODY, so passing "?seeds=…" there would send it as a GET payload).
    check GET "/v1/agents/sessions/$SESSION_ID/memory/relations/graph?seeds=$MEM_A" 200 '.items'
    check DELETE "/v1/agents/sessions/$SESSION_ID/memory/relations?from=$MEM_A" 200 '.deleted'
  else
    skip "memory/relations" "no memory fragment ids captured"
  fi
else
  skip "memory" "no session id captured"
fi

# ================================================================= SUBAGENTS
echo
echo "== Subagents"
if [ -n "$SESSION_ID" ]; then
  # The single-agent runtime always returns an empty list; sub-resource GETs
  # need a real subagent id which never exists here, so they are skipped.
  check GET "/v1/agents/sessions/$SESSION_ID/subagents" 200 '.data'
  skip "/v1/agents/sessions/$SESSION_ID/subagents/:id" \
       "single-agent runtime has no subagents (always 404)"
else
  skip "subagents" "no session id captured"
fi

# ============================================================= EVENTS (gated)
echo
echo "== Events / Stream"
if [ "$EVENTS_TEST" = "1" ] && [ -n "$SESSION_ID" ]; then
  check POST "/v1/agents/sessions/$SESSION_ID/events" 200 '.id' '{"input":"ping"}'
  TURN_ID="$(jqid '.id')"
  [ -n "$TURN_ID" ] && check GET "/v1/agents/sessions/$SESSION_ID/turns/$TURN_ID" 200 '.id'

  # SSE stream: expect HTTP 200 and at least one event:/data: frame.
  sse=(
    -sS --no-buffer --max-time 30 "${TLS_OPTS[@]}"
    -X POST -H "OpenAI-Beta: agents=v1" -H "Content-Type: application/json"
    -o "$SSE_FILE" -w '%{http_code}'
  )
  [ -n "$ADMIN_KEY" ] && sse+=(-H "Authorization: Bearer $ADMIN_KEY")
  SSE_CODE="$(curl "${sse[@]}" -d '{"input":"ping"}' \
    "$BASE_URL/v1/agents/sessions/$SESSION_ID/events/stream")"
  TOTAL=$((TOTAL + 1))
  if [ "$SSE_CODE" = "200" ] && grep -qE '^(event|data):' "$SSE_FILE"; then
    PASS=$((PASS + 1))
    printf "  PASS  POST  /v1/agents/sessions/%s/events/stream\n" "$SESSION_ID"
  else
    FAIL=$((FAIL + 1))
    printf "  FAIL  POST  /v1/agents/sessions/%s/events/stream (got HTTP %s)\n" \
      "$SESSION_ID" "$SSE_CODE"
  fi
else
  skip "/v1/agents/sessions/:id/events" "EVENTS_TEST!=1"
  skip "/v1/agents/sessions/:id/events/stream" "EVENTS_TEST!=1"
fi

# ============================================================ API KEYS (admin)
echo
echo "== API keys (admin)"
if [ -n "$ADMIN_KEY" ]; then
  check POST /v1/api-keys 200 '.id' '{"label":"smoke-key","scopes":["read","write"]}'
  KEY_ID="$(jqid '.id')"
  check GET /v1/api-keys 200 '.data'
  if [ -n "$KEY_ID" ]; then
    check DELETE "/v1/api-keys/$KEY_ID" 200 '.id'
  else
    skip "/v1/api-keys/:id" "no key id captured"
  fi
else
  skip "/v1/api-keys*" "service is OPEN (AGENT_CLOUD_API_KEY empty) — admin tests unavailable"
fi

# ============================================================ 501 STUB ROUTES
echo
echo "== 501 stubs"
STUBS=(
  "GET:/v1/vaults"
  "POST:/v1/vaults"
  "GET:/v1/vaults/vault_1"
  "GET:/v1/vaults/vault_1/credentials"
  "GET:/v1/agents/environments"
  "POST:/v1/agents/environments"
  "GET:/v1/agents/environments/env_1"
  "GET:/v1/agents/environments/env_1/files"
  "POST:/v1/agents/environments/env_1/files"
  "GET:/v1/agents/environments/templates"
)
if [ -n "$SESSION_ID" ]; then
  STUBS+=("GET:/v1/agents/sessions/$SESSION_ID/artifacts")
fi
for entry in "${STUBS[@]}"; do
  m="${entry%%:*}"; p="${entry#*:}"
  check "$m" "$p" 501 '.error'
done

# ============================================================== CLEANUP (agent)
echo
echo "== Cleanup"
if [ -n "$AGENT_ID" ]; then
  check DELETE "/v1/agents/$AGENT_ID" 200 '.deleted'
fi

# ------------------------------------------------------------------- summary
echo
echo "==================================================================="
echo "TOTAL $TOTAL | PASS $PASS | FAIL $FAIL | SKIP $SKIP"
if [ "$FAIL" -gt 0 ]; then
  echo "RESULT: FAILURES PRESENT"
else
  echo "RESULT: ALL CHECKED ENDPOINTS PASSED"
fi
exit "$FAIL"
