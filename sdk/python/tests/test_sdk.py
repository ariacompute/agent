"""Unit tests for the Python SDK (stdlib ``unittest``; no network)."""

from __future__ import annotations

import asyncio
import json
import sys
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ariacompute_agent import (  # noqa: E402
    Agent,
    ClientOptions,
    Runner,
    Session,
    function_tool,
)
from ariacompute_agent import transport  # noqa: E402


class FakeResponse:
    """Minimal stand-in for ``urllib``'s response object."""

    def __init__(self, payload: bytes) -> None:
        self._payload = payload
        self.closed = False

    def read(self, n: int = -1) -> bytes:
        if n is None or n < 0:
            out, self._payload = self._payload, b""
        else:
            out, self._payload = self._payload[:n], self._payload[n:]
        return out

    def close(self) -> None:
        self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


def sse_payload(frames: list[dict]) -> bytes:
    return "".join(f"data: {json.dumps(f)}\n\n" for f in frames).encode()


class AgentTest(unittest.TestCase):
    def test_requires_name(self):
        with self.assertRaises(ValueError):
            Agent(name="")

    def test_tool_schemas_drop_execute(self):
        @function_tool
        def history_fun_fact() -> str:
            """Return a short history fact."""
            return "Sharks are older than trees."

        agent = Agent(name="tutor", tools=[history_fun_fact])
        schemas = agent.tool_schemas()
        self.assertEqual(len(schemas), 1)
        self.assertEqual(schemas[0]["name"], "history_fun_fact")
        self.assertNotIn("execute", schemas[0])
        self.assertEqual(schemas[0]["description"], "Return a short history fact.")


class RunnerTest(unittest.TestCase):
    def test_run_creates_session_and_returns_final_output(self):
        calls: list[dict] = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append({"path": path, "body": body, "client": client})
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(json.dumps({"id": "turn_1", "output": "476 AD"}).encode())

        client = ClientOptions(base_url="http://aria.test", api_key="aria-test")
        agent = Agent(name="History tutor", client=client)
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            result = asyncio.run(Runner.run(agent, "When did the Roman Empire fall?"))

        self.assertEqual(result.final_output, "476 AD")
        self.assertEqual(result.session_id, "sess_1")
        self.assertEqual(result.turn_id, "turn_1")
        self.assertEqual(result.last_agent.name, "History tutor")
        self.assertEqual(calls[0]["body"], {"agent": "History tutor"})
        self.assertEqual(calls[0]["client"].beta_header, "agents=v1")
        self.assertEqual(calls[1]["path"], "/v1/agents/sessions/sess_1/events")
        self.assertEqual(calls[1]["body"], {"input": "When did the Roman Empire fall?"})

    def test_run_reuses_session(self):
        calls: list[dict] = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append(path)
            return FakeResponse(json.dumps({"id": "turn_2", "output": "second"}).encode())

        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            result = asyncio.run(Runner.run(agent, "again", session="sess_existing"))
        self.assertEqual(result.session_id, "sess_existing")
        self.assertEqual(calls, ["/v1/agents/sessions/sess_existing/events"])

    def test_run_streamed_collects_deltas(self):
        frames = [
            {"type": "agent.turn.created", "turn_id": "turn_1", "sequence_number": 1},
            {"type": "agent.turn.output_text.delta", "delta": "4", "sequence_number": 2},
            {"type": "agent.turn.output_text.delta", "delta": "76", "sequence_number": 3},
            {
                "type": "agent.turn.completed",
                "turn": {"id": "turn_1", "status": "completed", "output": "476"},
                "sequence_number": 4,
            },
        ]

        def fake_request(client, method, path, body=None, stream=False):
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(sse_payload(frames))

        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "Rome?")
                text = ""
                types = []
                async for ev in streamed.events:
                    types.append(ev["type"])
                    if ev["type"] == "agent.turn.output_text.delta":
                        text += ev["delta"]
                result = await streamed.completed
            return text, types, result

        text, types, result = asyncio.run(main())
        self.assertEqual(text, "476")
        self.assertEqual(types[0], "agent.turn.created")
        self.assertEqual(result.final_output, "476")
        self.assertEqual(result.turn_id, "turn_1")
        self.assertEqual(result.to_input_list()[0]["content"], "Rome?")

    def test_run_streamed_raises_on_failure(self):
        def fake_request(client, method, path, body=None, stream=False):
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(
                sse_payload([{"type": "agent.turn.failed", "error": {"message": "boom"}}])
            )

        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "Rome?")
                await streamed.completed

        with self.assertRaises(RuntimeError):
            asyncio.run(main())


class SseTest(unittest.TestCase):
    def test_parses_frames_and_ignores_done(self):
        raw = (
            ": keep-alive\n\n"
            'data: {"type":"agent.turn.output_text.delta","delta":"a"}\n\n'
            "data: [DONE]\n\n"
            'data: {"type":"agent.turn.completed"}\n\n'
        ).encode()
        frames = list(transport.iter_sse(FakeResponse(raw)))
        self.assertEqual(
            [f["type"] for f in frames],
            ["agent.turn.output_text.delta", "agent.turn.completed"],
        )


class SessionMemoryTest(unittest.TestCase):
    def test_memorize_posts_key_value(self):
        calls = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append((method, path, body))
            return FakeResponse(json.dumps({"key": "user_name", "value": "Ada"}).encode())

        session = Session("sess_1", ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            session.memorize("user_name", "Ada")
            self.assertEqual(session.recall("user_name"), "Ada")
        self.assertEqual(
            calls[0],
            (
                "POST",
                "/v1/agents/sessions/sess_1/memory",
                {"key": "user_name", "value": "Ada", "kind": "long_term"},
            ),
        )
        self.assertEqual(calls[1][0], "GET")

    def test_recall_missing_key_returns_none(self):
        def fake_request(client, method, path, body=None, stream=False):
            return FakeResponse(json.dumps({"key": "ghost"}).encode())

        session = Session("sess_1", ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            self.assertIsNone(session.recall("ghost"))


class SessionTest(unittest.TestCase):
    def test_create_posts_agent_reference(self):
        calls: list[dict] = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append(body)
            return FakeResponse(json.dumps({"id": "sess_9"}).encode())

        agent = Agent(
            name="tutor",
            instructions="be brief",
            client=ClientOptions(base_url="http://aria.test"),
        )
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            session = Session.create(agent)
        self.assertEqual(session.id, "sess_9")
        self.assertEqual(calls[0], {"agent": "tutor", "instructions": "be brief"})


if __name__ == "__main__":
    unittest.main()
