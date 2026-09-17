"""``Session`` — a server-side agent session plus its memory backend."""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional, Union

from .memory import (
    CompositeMemoryStore,
    MemoryBackend,
    build_store,
)
from .transport import get_json, post_json, resolve_client
from .types import ClientOptions, HistoryInput

if TYPE_CHECKING:  # pragma: no cover
    from .agent import Agent


class Session:
    """Carries the conversation state (and memory backend) across runs.

    ```python
    session = await Session.create(agent)
    result = await Runner.run(agent, "hello", session=session)
    ```
    """

    def __init__(
        self,
        id: str,
        client: Optional[ClientOptions] = None,
        history: Optional[list[HistoryInput]] = None,
        backend: Union[str, MemoryBackend, None] = None,
        memo_db: Optional[str] = None,
    ) -> None:
        self.id = id
        self.client = client or ClientOptions()
        self.history: list[HistoryInput] = history or []
        self.backend = MemoryBackend.parse(backend) if backend is not None else MemoryBackend.CLOUD
        self.memo_db = memo_db
        self._default = build_store(self.backend, self.id, self.client, self.memo_db)

    @classmethod
    def create(cls, agent: "Agent", client: Optional[ClientOptions] = None) -> "Session":
        """Create a session bound to ``agent`` (the agent must exist in the cloud)."""
        from .agent import Agent  # local import to avoid a cycle

        assert isinstance(agent, Agent)
        resolved = resolve_client(client or agent.client)
        body: dict[str, object] = {}
        if agent.id:
            body["agent_id"] = agent.id
        else:
            body["agent"] = agent.name
        if agent.instructions:
            body["instructions"] = agent.instructions
        res = post_json(resolved, "/v1/agents/sessions", body)
        return cls(
            res["id"],
            client or agent.client,
            backend=agent.memory_backend,
            memo_db=agent.memo_db,
        )

    def _store(self, backend: Union[str, MemoryBackend, None]) -> object:
        if backend is None:
            return self._default
        parsed = MemoryBackend.parse(backend)
        if parsed is self.backend:
            return self._default
        if parsed is MemoryBackend.BOTH:
            return CompositeMemoryStore(
                *self._sides(),
            )
        return build_store(parsed, self.id, self.client, self.memo_db)

    def _sides(self) -> tuple:
        from .memory import CloudMemoryStore, LocalMemoryStore

        return (
            LocalMemoryStore(self.memo_db),
            CloudMemoryStore(self.id, self.client),
        )

    def memorize(
        self, key: str, value: str, backend: Union[str, MemoryBackend, None] = None
    ) -> None:
        """Write a keyed long-term memory.

        ``backend`` (``cloud`` / ``local`` / ``both``) overrides the session
        default for this call. ``local`` writes to the aria memo (SQLite) store.
        """
        self._store(backend).put(key, value)

    def recall(
        self, key: str, backend: Union[str, MemoryBackend, None] = None
    ) -> Optional[str]:
        """Read a keyed long-term memory (missing keys return ``None``)."""
        return self._store(backend).get(key)

    def record(self, user_input: HistoryInput, output: str) -> None:
        """Append the exchange to the client-side history mirror."""
        self.history.append(user_input)
        self.history.append({"role": "assistant", "content": output})
