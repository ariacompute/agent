"""Offline evaluation harness for the agent SDK (js / python).

Mirrors the memo `benches/` layout: zero network, zero LLM. Every case below
drives the SDK through an injected transport / in-memory store and asserts the
decoded result, so it runs on a laptop with no cloud or OpenAI key.

A judge (LLM) metric is declared but skipped with a `reason` unless
``BENCH_LLM_API_KEY`` is set — scores are never fabricated.

Run:
    python benches/run.py            # only the offline fixture cases
    python benches/run.py --bench js # (reserved) JS golden case via `node`
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
import tempfile
import time
import urllib.error
from pathlib import Path
from typing import Any
from unittest import mock

HERE = Path(__file__).resolve().parent
# Make the python SDK importable without an install.
sys.path.insert(0, str(HERE.parent / "sdk" / "python"))

from ariacompute_agent import (  # noqa: E402
    Agent,
    AriaError,
    AriaErrorKind,
    ClientOptions,
    LocalMemoryStore,
    Runner,
    Session,
    function_tool,
)
from ariacompute_agent import transport  # noqa: E402


def _sse_payload(frames: list[dict[str, Any]]) -> bytes:
    return "".join(f'data: {json.dumps(f)}\n\n' for f in frames).encode()


def _fake_response(body: bytes, content_type: str = "application/json") -> Any:
    state = {"read": False}

    def read(self, *a):
        if state["read"]:
            return b""
        state["read"] = True
        return body

    cls = type(
        "FakeResponse",
        (),
        {
            "status": 200,
            "headers": {"Content-Type": content_type},
            "read": read,
            "close": lambda self: None,
            "json": lambda self: json.loads(body.decode()),
            "__enter__": lambda self: self,
            "__exit__": lambda self, *a: None,
        },
    )
    return cls()


# --------------------------------------------------------------------------- #
# Offline cases
# --------------------------------------------------------------------------- #


def case_sse_decode_and_run() -> dict[str, Any]:
    """A canned SSE stream must decode into the expected RunResult."""
    frames = [
        {"type": "agent.turn.created", "turn_id": "t1", "sequence_number": 1},
        {"type": "agent.turn.output_text.delta", "delta": "The ", "sequence_number": 2},
        {"type": "agent.turn.output_text.delta", "delta": "answer", "sequence_number": 3},
        {
            "type": "agent.turn.output_text.done",
            "text": "The answer",
            "sequence_number": 4,
        },
        {
            "type": "agent.turn.completed",
            "turn": {"id": "t1", "output": "The answer"},
            "sequence_number": 5,
        },
    ]

    def fake_request(client, method, path, body=None, stream=False):
        if path.endswith("/v1/agents/sessions"):
            return _fake_response(json.dumps({"id": "sess_bench"}).encode())
        return _fake_response(_sse_payload(frames), "text/event-stream")

    agent = Agent(name="bench", client=ClientOptions(base_url="http://x", api_key="k"))
    with mock.patch.object(transport, "_request", side_effect=fake_request):
        streamed = asyncio.run(Runner.run_streamed(agent, "question?"))
        result = asyncio.run(streamed.completed)
    ok = result.final_output == "The answer" and result.session_id == "sess_bench"
    return {"name": "sse_decode_and_run", "value": 1.0 if ok else 0.0, "requires_llm": False}


def case_sse_failed_mapped_to_api_error() -> dict[str, Any]:
    """A failed SSE frame surfaces a typed api error, not a generic one."""
    frames = [{"type": "agent.turn.failed", "error": {"message": "boom"}, "sequence_number": 1}]

    def fake_request(client, method, path, body=None, stream=False):
        if path.endswith("/v1/agents/sessions"):
            return _fake_response(json.dumps({"id": "sess_bench"}).encode())
        return _fake_response(_sse_payload(frames), "text/event-stream")

    agent = Agent(name="bench", client=ClientOptions(base_url="http://x", api_key="k"))
    with mock.patch.object(transport, "_request", side_effect=fake_request):
        streamed = asyncio.run(Runner.run_streamed(agent, "q"))
        try:
            asyncio.run(streamed.completed)
            ok = False
        except Exception as e:  # noqa: BLE001
            ok = isinstance(e, AriaError) and e.kind == AriaErrorKind.API
    return {"name": "sse_failed_api_error", "value": 1.0 if ok else 0.0, "requires_llm": False}


def case_memory_local_roundtrip() -> dict[str, Any]:
    """Local (aria memo) store persists and recalls a fact offline."""
    with tempfile.TemporaryDirectory() as tmp:
        store = LocalMemoryStore(str(Path(tmp) / "memo.db"))
        store.put("user_name", "Ada")
        got = store.get("user_name")
        ok = got == "Ada"
    return {"name": "memory_local_roundtrip", "value": 1.0 if ok else 0.0, "requires_llm": False}


def case_session_readonly_endpoints() -> dict[str, Any]:
    """get_items / get_turns hit the read-only endpoints (no context mutation)."""
    calls: list[tuple[str, str]] = []

    def fake_request(client, method, path, body=None, stream=False):
        calls.append((method, path))
        if path.endswith("/items"):
            return _fake_response(json.dumps([{"id": "i1"}]).encode())
        return _fake_response(json.dumps([{"id": "t1"}]).encode())

    session = Session("sess_bench", ClientOptions(base_url="http://x", api_key="k"))
    with mock.patch.object(transport, "_request", side_effect=fake_request):
        items = session.get_items()
        turns = session.get_turns()
    ok = (
        items == [{"id": "i1"}]
        and turns == [{"id": "t1"}]
        and calls[0] == ("GET", "/v1/agents/sessions/sess_bench/items")
        and calls[1][0] == "GET"
    )
    return {"name": "session_readonly_endpoints", "value": 1.0 if ok else 0.0, "requires_llm": False}


OFFLINE_CASES = [
    case_sse_decode_and_run,
    case_sse_failed_mapped_to_api_error,
    case_memory_local_roundtrip,
    case_session_readonly_endpoints,
]


def judge_metric(scores: list[dict[str, Any]]) -> dict[str, Any]:
    """Optional LLM judge; skipped unless credentials are present."""
    if not os.environ.get("BENCH_LLM_API_KEY"):
        return {
            "name": "judge_coherence",
            "value": None,
            "requires_llm": True,
            "status": "skipped",
            "reason": "BENCH_LLM_API_KEY not set; LLM judge disabled offline",
        }
    # Intentionally minimal: a real judge would call the OpenAI-compatible API.
    return {"name": "judge_coherence", "value": None, "requires_llm": True, "status": "skipped",
            "reason": "judge not implemented in this offline harness"}


def main() -> int:
    scores = [case() for case in OFFLINE_CASES]
    scores.append(judge_metric(scores))

    passed = sum(1 for s in scores if not s.get("requires_llm") and s.get("value") == 1.0)
    report = {
        "name": "agent-sdk-offline",
        "generated_at": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "dataset_source": "fixture",
        "systems": [
            {
                "system": "ariacompute-agent (python)",
                "dataset_source": "fixture",
                "offline_passed": passed,
                "offline_total": len([s for s in scores if not s.get("requires_llm")]),
                "scores": scores,
            }
        ],
    }

    out_dir = HERE / "results" / time.strftime("%Y%m%dT%H%M%SZ", time.gmtime())
    out_dir.mkdir(parents=True, exist_ok=True)
    out_file = out_dir / "sdk_offline.json"
    out_file.write_text(json.dumps(report, indent=2), encoding="utf-8")
    print(json.dumps(report, indent=2))
    print(f"\nwrote {out_file}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
