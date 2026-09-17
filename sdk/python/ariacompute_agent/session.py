"""``Session`` — a server-side agent session (``/v1/agents/sessions``)."""

from __future__ import annotations

from typing import TYPE_CHECKING, Optional

from .transport import get_json, post_json, resolve_client
from .types import ClientOptions, HistoryInput

if TYPE_CHECKING:  # pragma: no cover
    from .agent import Agent


class Session:
    """Carries the conversation state across runs.

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
    ) -> None:
        self.id = id
        self.client = client or ClientOptions()
        self.history: list[HistoryInput] = history or []

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
        return cls(res["id"], client or agent.client)

    def memorize(self, key: str, value: str) -> None:
        """Write a keyed long-term memory for this session.

        The value is persisted in the session's context store (Postgres +
        pgvector on the cloud, the on-device store for native SDKs) and is
        recallable by every later turn of the session — even after a restart.
        """
        resolved = resolve_client(self.client)
        post_json(resolved, f"/v1/agents/sessions/{self.id}/memory", {"key": key, "value": value})

    def recall(self, key: str) -> Optional[str]:
        """Read a keyed long-term memory (missing keys return ``None``)."""
        resolved = resolve_client(self.client)
        res = get_json(resolved, f"/v1/agents/sessions/{self.id}/memory/{key}")
        return res.get("value")

    def record(self, user_input: HistoryInput, output: str) -> None:
        """Append the exchange to the client-side history mirror."""
        self.history.append(user_input)
        self.history.append({"role": "assistant", "content": output})
