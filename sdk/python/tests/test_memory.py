"""Backend switching tests for the Python SDK (no network, no CLI)."""

from __future__ import annotations

import json
import os
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))

from ariacompute_agent import (  # noqa: E402
    Agent,
    CloudMemoryStore,
    CompositeMemoryStore,
    LocalMemoryStore,
    MemoryBackend,
    Session,
)
from ariacompute_agent import transport  # noqa: E402
from ariacompute_agent.types import ClientOptions  # noqa: E402


class FakeResponse:
    def __init__(self, payload: bytes) -> None:
        self._payload = payload

    def read(self, n: int = -1) -> bytes:
        if n is None or n < 0:
            out, self._payload = self._payload, b""
        else:
            out, self._payload = self._payload[:n], self._payload[n:]
        return out

    def close(self) -> None:
        pass

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()
        return False


def json_response(body: dict) -> FakeResponse:
    return FakeResponse(json.dumps(body).encode())


class MemoryBackendEnumTest(unittest.TestCase):
    def test_parse_accepts_strings_and_members(self):
        self.assertEqual(MemoryBackend.parse("cloud"), MemoryBackend.CLOUD)
        self.assertEqual(MemoryBackend.parse(" LOCAL "), MemoryBackend.LOCAL)
        self.assertEqual(MemoryBackend.parse("both"), MemoryBackend.BOTH)
        self.assertEqual(MemoryBackend.parse(None), MemoryBackend.CLOUD)
        self.assertEqual(MemoryBackend.parse(MemoryBackend.BOTH), MemoryBackend.BOTH)
        with self.assertRaises(ValueError):
            MemoryBackend.parse("nonsense")


class LocalMemoryStoreTest(unittest.TestCase):
    def test_roundtrip_uses_the_aria_memo_schema(self):
        with tempfile.TemporaryDirectory() as tmp:
            db = os.path.join(tmp, "memo.db")
            store = LocalMemoryStore(db)
            store.put("user_name", "Ada")
            self.assertEqual(store.get("user_name"), "Ada")
            self.assertIsNone(store.get("ghost"))

            import sqlite3

            conn = sqlite3.connect(db)
            row = conn.execute(
                "SELECT id, memo_type, content, metadata FROM memories WHERE deleted = 0"
            ).fetchone()
            conn.close()
            self.assertEqual(row[1], "long_term:semantic")
            self.assertEqual(row[2], "Ada")
            self.assertEqual(json.loads(row[3])["key"], "user_name")

    def test_put_overwrites_the_same_key(self):
        with tempfile.TemporaryDirectory() as tmp:
            store = LocalMemoryStore(os.path.join(tmp, "memo.db"))
            store.put("k", "v1")
            store.put("k", "v2")
            self.assertEqual(store.get("k"), "v2")


class CloudMemoryStoreTest(unittest.TestCase):
    def test_put_and_get_against_the_rest_endpoints(self):
        calls = []

        def fake_request(client, method, path, body=None, stream=False):
            calls.append((method, path, body))
            return json_response({"key": "k", "value": "from-cloud"})

        store = CloudMemoryStore("sess_1", ClientOptions(base_url="http://aria.test"))
        with mock.patch.object(transport, "_request", side_effect=fake_request):
            store.put("k", "v")
            self.assertEqual(store.get("k"), "from-cloud")
        self.assertEqual(
            calls[0],
            ("POST", "/v1/agents/sessions/sess_1/memory",
             {"key": "k", "value": "v", "kind": "long_term"}),
        )
        self.assertEqual(calls[1][1], "/v1/agents/sessions/sess_1/memory/k")


class CompositeMemoryStoreTest(unittest.TestCase):
    def test_writes_both_and_prefers_cloud_on_read(self):
        writes = []

        class Stub:
            def __init__(self, backend, value):
                self.backend = backend
                self.value = value

            def put(self, key, value):
                writes.append((self.backend.value, value))

            def get(self, key):
                return self.value

        composite = CompositeMemoryStore(
            Stub(MemoryBackend.LOCAL, "from-local"), Stub(MemoryBackend.CLOUD, "from-cloud")
        )
        self.assertEqual(composite.backend, MemoryBackend.BOTH)
        composite.put("k", "v")
        self.assertEqual(writes, [("local", "v"), ("cloud", "v")])
        self.assertEqual(composite.get("k"), "from-cloud")

    def test_falls_back_to_local_when_cloud_fails(self):
        class Broken:
            backend = MemoryBackend.CLOUD

            def put(self, key, value):
                raise RuntimeError("cloud down")

            def get(self, key):
                raise RuntimeError("cloud down")

        with tempfile.TemporaryDirectory() as tmp:
            local = LocalMemoryStore(os.path.join(tmp, "memo.db"))
            local.put("k", "from-local")
            composite = CompositeMemoryStore(local, Broken())
            composite.put("k", "kept-locally")
            self.assertEqual(composite.get("k"), "kept-locally")

    def test_errors_only_when_every_backend_fails(self):
        class Broken:
            backend = MemoryBackend.CLOUD

            def put(self, key, value):
                raise RuntimeError("nope")

            def get(self, key):
                raise RuntimeError("nope")

        composite = CompositeMemoryStore(Broken(), Broken())
        with self.assertRaises(RuntimeError):
            composite.put("k", "v")
        with self.assertRaises(RuntimeError):
            composite.get("k")


class SessionBackendTest(unittest.TestCase):
    def test_default_is_cloud_and_override_reaches_local(self):
        with tempfile.TemporaryDirectory() as tmp:
            session = Session(
                "sess_1",
                ClientOptions(base_url="http://aria.test"),
                memo_db=os.path.join(tmp, "memo.db"),
            )
            self.assertEqual(session.backend, MemoryBackend.CLOUD)

            # Explicit local override writes to aria memo, no HTTP involved.
            with mock.patch.object(transport, "_request") as req:
                session.memorize("k", "local-value", backend="local")
                self.assertEqual(session.recall("k", backend="local"), "local-value")
                req.assert_not_called()

    def test_unknown_backend_override_is_rejected(self):
        session = Session("sess_1", ClientOptions(base_url="http://aria.test"))
        with self.assertRaises(ValueError):
            session.recall("k", backend="nonsense")

    def test_session_create_inherits_agent_backend(self):
        def fake_request(client, method, path, body=None, stream=False):
            if path.endswith("/v1/agents/sessions"):
                return json_response({"id": "sess_9"})
            return json_response({"key": "k", "value": "v"})

        agent = Agent(
            name="tutor",
            client=ClientOptions(base_url="http://aria.test"),
            memory_backend="both",
            memo_db=":ignored-in-this-test:",
        )
        with tempfile.TemporaryDirectory() as tmp:
            agent.memo_db = os.path.join(tmp, "memo.db")
            with mock.patch.object(transport, "_request", side_effect=fake_request):
                session = Session.create(agent)
        self.assertEqual(session.id, "sess_9")
        self.assertEqual(session.backend, MemoryBackend.BOTH)


if __name__ == "__main__":
    unittest.main()
