"""Unit tests for the Python SDK (stdlib ``unittest``; no network)."""

from __future__ import annotations

import asyncio
import io
import json
import sys
import unittest
import urllib.error
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ariacompute_agent import (  # noqa: E402
    Agent,
    AriaError,
    AriaErrorKind,
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
        with self.assertRaises(ValueError):
            Agent(name="   ")

    def test_rejects_duplicate_tool_name(self):
        shared = function_tool(lambda: "x")  # type: ignore
        with self.assertRaises(ValueError):
            Agent(name="tutor", tools=[shared, shared])

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

    def test_run_streamed_collects_usage_and_approvals(self):
        frames = [
            {"type": "agent.turn.created", "turn_id": "turn_9", "sequence_number": 1},
            {
                "type": "agent.turn.approval.requested",
                "approval_id": "call_1",
                "tool": "shell",
                "sequence_number": 2,
            },
            {
                "type": "agent.turn.approval.resolved",
                "approval_id": "call_1",
                "tool": "shell",
                "decision": "unavailable",
                "reason": "no approval provider configured",
                "sequence_number": 3,
            },
            {
                "type": "agent.turn.usage",
                "usage": {"input_tokens": 9, "output_tokens": 4, "total_tokens": 13},
                "model": "gpt-4o-mini",
                "sequence_number": 4,
            },
            {
                "type": "agent.turn.completed",
                "turn": {"id": "turn_9", "status": "completed", "output": "denied"},
                "sequence_number": 5,
            },
        ]

        def fake_request(client, method, path, body=None, stream=False):
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(sse_payload(frames))

        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "go")
                seen = []
                decisions = []
                async for ev in streamed.events:
                    seen.append(ev["type"])
                    if ev["type"] == "agent.turn.approval.resolved":
                        decisions.append(ev["decision"])
                result = await streamed.completed
            return seen, decisions, result

        seen, decisions, result = asyncio.run(main())
        self.assertIn("agent.turn.approval.requested", seen)
        self.assertEqual(decisions, ["unavailable"])
        self.assertIsNotNone(result.usage)
        self.assertEqual(result.usage.input_tokens, 9)
        self.assertEqual(result.usage.output_tokens, 4)
        self.assertEqual(result.usage.total_tokens, 13)
        self.assertEqual(result.usage.model, "gpt-4o-mini")

    def test_a_stream_without_usage_reports_none(self):
        def fake_request(client, method, path, body=None, stream=False):
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(
                sse_payload(
                    [{"type": "agent.turn.output_text.done", "text": "hi", "sequence_number": 1}]
                )
            )

        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                return await (await Runner.run_streamed(agent, "go")).completed

        self.assertIsNone(asyncio.run(main()).usage)

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

        with self.assertRaises(AriaError) as ctx:
            asyncio.run(main())
        self.assertEqual(ctx.exception.kind, AriaErrorKind.API)


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

    def test_iter_sse_ignores_malformed_json(self):
        raw = (
            ": keep-alive\n\n"
            'data: not-json\n\n'
            'data: {"type":"agent.turn.created"}\n\n'
        ).encode()
        frames = list(transport.iter_sse(FakeResponse(raw)))
        self.assertEqual([f["type"] for f in frames], ["agent.turn.created"])


class RunnerMaxTurnsTest(unittest.TestCase):
    def _fake(self, frames=None):
        captured = {}

        def fake_request(client, method, path, body=None, stream=False):
            captured["path"] = path
            if path.endswith("/v1/agents/sessions"):
                return FakeResponse(json.dumps({"id": "sess_1"}).encode())
            return FakeResponse(sse_payload(frames or []))

        return fake_request, captured

    def test_run_rejects_max_turns_zero_before_request(self):
        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=self._fake()[0]):
            with self.assertRaises(AriaError) as ctx:
                asyncio.run(Runner.run(agent, "hi", max_turns=0))
        self.assertEqual(ctx.exception.kind, AriaErrorKind.CONFIG)

    def test_run_streamed_caps_max_turns(self):
        frames = [
            {"type": "agent.turn.created", "turn_id": "t1", "sequence_number": 1},
            {"type": "agent.turn.output_text.done", "text": "first", "sequence_number": 2},
            {"type": "agent.turn.completed", "turn": {"id": "t1"}, "sequence_number": 3},
            {"type": "agent.turn.created", "turn_id": "t2", "sequence_number": 4},
        ]
        fake_request, _ = self._fake(frames)
        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "go", max_turns=1)
                await streamed.completed

        with self.assertRaises(AriaError) as ctx:
            asyncio.run(main())
        self.assertEqual(ctx.exception.kind, AriaErrorKind.CONFIG)
        self.assertIn("exceeded maxTurns", str(ctx.exception))

    def test_empty_stream_is_an_error(self):
        fake_request, _ = self._fake([])
        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "go")
                await streamed.completed

        with self.assertRaises(AriaError) as ctx:
            asyncio.run(main())
        self.assertEqual(ctx.exception.kind, AriaErrorKind.UNKNOWN)
        self.assertIn("without any event", str(ctx.exception))

    def test_failed_frame_surfaces_typed_api_error(self):
        frames = [{"type": "agent.turn.failed", "error": {"message": "boom"}}]
        fake_request, _ = self._fake(frames)
        agent = Agent(name="tutor", client=ClientOptions(base_url="http://aria.test"))

        async def main():
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                streamed = await Runner.run_streamed(agent, "go")
                await streamed.completed

        with self.assertRaises(AriaError) as ctx:
            asyncio.run(main())
        self.assertEqual(ctx.exception.kind, AriaErrorKind.API)


class TransportErrorTest(unittest.TestCase):
    def test_maps_http_status_to_auth_or_api(self):
        base = ClientOptions(base_url="http://aria.test", api_key="k")
        for code, kind in ((401, AriaErrorKind.AUTH), (404, AriaErrorKind.API)):
            err = urllib.error.HTTPError(
                url="http://aria.test/x",
                code=code,
                msg="",
                hdrs=None,
                fp=io.BytesIO(b'{"error":"denied"}'),
            )
            with mock.patch.object(transport.urllib.request, "urlopen", side_effect=err):
                with self.assertRaises(AriaError) as ctx:
                    transport.post_json(base, "/x", {"a": 1})
            self.assertEqual(ctx.exception.kind, kind)
            self.assertEqual(ctx.exception.status, code)
            self.assertIn('{"error":"denied"}', str(ctx.exception))

    def test_http_error_with_str_body_is_normalized(self):
        # Debian's patched urllib can return str from HTTPError.read().
        base = ClientOptions(base_url="http://aria.test")
        err = urllib.error.HTTPError(
            url="http://aria.test/x",
            code=401,
            msg="",
            hdrs=None,
            fp=io.StringIO("unauthorized"),
        )
        with mock.patch.object(transport.urllib.request, "urlopen", side_effect=err):
            with self.assertRaises(AriaError) as ctx:
                transport.get_json(base, "/x")
        self.assertEqual(ctx.exception.kind, AriaErrorKind.AUTH)
        self.assertEqual(ctx.exception.status, 401)
        self.assertIn("unauthorized", str(ctx.exception))

    def test_maps_url_error_to_network(self):
        base = ClientOptions(base_url="http://aria.test", api_key="k")
        err = urllib.error.URLError(reason="conn reset")
        with mock.patch.object(transport.urllib.request, "urlopen", side_effect=err):
            with self.assertRaises(AriaError) as ctx:
                transport.get_json(base, "/x")
        self.assertEqual(ctx.exception.kind, AriaErrorKind.NETWORK)


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


    def test_get_title_requests_the_title_endpoint(self):
        calls: list[tuple[str, str]] = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append((method, path))
            return FakeResponse(
                json.dumps(
                    {
                        "object": "agent.session.title",
                        "session_id": "sess_1",
                        "title": "summarise the repo",
                        "source": "first_prompt",
                        "updated_at": 7,
                    }
                ).encode()
            )

        session = Session("sess_1", ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            title = session.get_title()

        self.assertEqual(calls, [("GET", "/v1/agents/sessions/sess_1/title")])
        self.assertEqual(title.object, "agent.session.title")
        self.assertEqual(title.session_id, "sess_1")
        self.assertEqual(title.title, "summarise the repo")
        self.assertEqual(title.source, "first_prompt")
        self.assertEqual(title.updated_at, 7)


if __name__ == "__main__":
    unittest.main()
