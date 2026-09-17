"""Memory backends for :mod:`ariacompute_agent`.

* ``cloud`` — the agent-cloud session memory REST endpoints
  (``/v1/agents/sessions/{id}/memory``).
* ``local`` — **aria memo**: the same SQLite ``memories`` database the
  ``aria-memo`` product/CLI uses, written directly through the stdlib
  :mod:`sqlite3` module (no external binary required). ``aria-memo list --json``
  can read the very same file.
* ``both`` — writes to local **and** cloud; reads merged (cloud wins, local is
  the fallback).
"""

from __future__ import annotations

import json
import sqlite3
from enum import Enum
from pathlib import Path
from typing import Optional, Protocol

from .transport import get_json, post_json, resolve_client
from .types import ClientOptions


class MemoryBackend(str, Enum):
    """Where a memory context lives."""

    CLOUD = "cloud"
    LOCAL = "local"
    BOTH = "both"

    @classmethod
    def parse(cls, value: Optional[object]) -> "MemoryBackend":
        if value is None:
            return cls.CLOUD
        if isinstance(value, cls):
            return value
        raw = str(getattr(value, "value", value)).strip().lower()
        for member in cls:
            if member.value == raw:
                return member
        raise ValueError(f"unknown memory backend: {value}")


class MemoryStore(Protocol):
    """Key/value view of a memory backend."""

    backend: MemoryBackend

    def put(self, key: str, value: str) -> None:  # pragma: no cover - protocol
        ...

    def get(self, key: str) -> Optional[str]:  # pragma: no cover - protocol
        ...


# --- aria memo (local) -------------------------------------------------------

#: aria memo's ``memories`` schema — kept byte-identical so its CLI can open the file.
ARIA_MEMO_SCHEMA = """
CREATE TABLE IF NOT EXISTS memories (
    id TEXT PRIMARY KEY,
    memo_type TEXT NOT NULL,
    content TEXT NOT NULL,
    embedding BLOB,
    metadata TEXT NOT NULL,
    importance REAL NOT NULL,
    version INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    deleted INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_memories_type ON memories(memo_type);
CREATE INDEX IF NOT EXISTS idx_memories_updated_at ON memories(updated_at);
CREATE INDEX IF NOT EXISTS idx_memories_deleted ON memories(deleted);
"""

MEMO_TYPE_LONG_TERM = "long_term:semantic"


def _now_sec() -> int:
    import time

    return int(time.time())


class LocalMemoryStore:
    """``local`` backend: aria memo (SQLite, same format as ``aria-memo``)."""

    backend = MemoryBackend.LOCAL

    def __init__(self, db_path: Optional[str] = None) -> None:
        path = db_path or __import__("os").environ.get("ARIA_MEMO_DB") or "memo.db"
        self.db_path = str(path)
        parent = Path(self.db_path).parent
        if str(parent) and not parent.exists():
            parent.mkdir(parents=True, exist_ok=True)
        self._ensure_schema()

    def _connect(self) -> sqlite3.Connection:
        conn = sqlite3.connect(self.db_path)
        conn.execute("PRAGMA journal_mode=WAL")
        return conn

    def _ensure_schema(self) -> None:
        with self._connect() as conn:
            conn.executescript(ARIA_MEMO_SCHEMA)

    def put(self, key: str, value: str) -> None:
        """Write ``key`` as a long-term aria memo (``metadata.key`` holds the key)."""
        now = _now_sec()
        metadata = json.dumps({"key": key})
        with self._connect() as conn:
            conn.execute(
                "INSERT INTO memories "
                "(id, memo_type, content, embedding, metadata, importance, version, "
                " created_at, updated_at, deleted) "
                "VALUES (?, ?, ?, ?, ?, ?, 1, ?, ?, 0) "
                "ON CONFLICT(id) DO UPDATE SET "
                "  content = excluded.content, metadata = excluded.metadata, "
                "  updated_at = excluded.updated_at, deleted = 0",
                (f"key:{key}", MEMO_TYPE_LONG_TERM, value, b"", metadata, 0.8, now, now),
            )

    def get(self, key: str) -> Optional[str]:
        with self._connect() as conn:
            rows = conn.execute(
                "SELECT content, metadata FROM memories WHERE deleted = 0",
            ).fetchall()
        for content, metadata in rows:
            try:
                parsed = json.loads(metadata or "{}")
            except json.JSONDecodeError:
                parsed = {}
            if parsed.get("key") == key:
                return content
        return None


# --- cloud -------------------------------------------------------------------


class CloudMemoryStore:
    """``cloud`` backend: the agent-cloud session memory REST endpoints."""

    backend = MemoryBackend.CLOUD

    def __init__(self, session_id: str, client: Optional[ClientOptions] = None) -> None:
        self.session_id = session_id
        self.client = client or ClientOptions()

    def _base(self) -> str:
        return f"/v1/agents/sessions/{self.session_id}/memory"

    def put(self, key: str, value: str) -> None:
        resolved = resolve_client(self.client)
        post_json(resolved, self._base(), {"key": key, "value": value, "kind": "long_term"})

    def get(self, key: str) -> Optional[str]:
        resolved = resolve_client(self.client)
        res = get_json(resolved, f"{self._base()}/{key}")
        return res.get("value")


# --- both --------------------------------------------------------------------


class CompositeMemoryStore:
    """``both`` backend: writes to local **and** cloud, reads merged."""

    backend = MemoryBackend.BOTH

    def __init__(self, local: MemoryStore, cloud: MemoryStore) -> None:
        self.local = local
        self.cloud = cloud

    def put(self, key: str, value: str) -> None:
        errors = []
        for store in (self.local, self.cloud):
            try:
                store.put(key, value)
            except Exception as e:  # noqa: BLE001 - one side may be offline
                errors.append(str(e))
        if len(errors) == 2:
            raise RuntimeError("memory write failed on every backend: " + "; ".join(errors))

    def get(self, key: str) -> Optional[str]:
        errors = []
        for store in (self.cloud, self.local):
            try:
                value = store.get(key)
            except Exception as e:  # noqa: BLE001 - fall through to the other side
                errors.append(str(e))
                continue
            if value is not None:
                return value
        if len(errors) == 2:
            raise RuntimeError("memory read failed on every backend: " + "; ".join(errors))
        return None


def build_store(
    backend: MemoryBackend,
    session_id: str,
    client: Optional[ClientOptions] = None,
    memo_db: Optional[str] = None,
) -> MemoryStore:
    """Build the store for ``backend`` (local is created lazily but validated)."""
    if backend is MemoryBackend.LOCAL:
        return LocalMemoryStore(memo_db)
    if backend is MemoryBackend.CLOUD:
        return CloudMemoryStore(session_id, client)
    return CompositeMemoryStore(
        LocalMemoryStore(memo_db),
        CloudMemoryStore(session_id, client),
    )
