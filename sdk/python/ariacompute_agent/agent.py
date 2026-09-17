"""``Agent`` — mirrors ``Agent(...)`` from the OpenAI Agents SDK."""

from __future__ import annotations

from typing import Any, Optional, Sequence, Union

from .memory import MemoryBackend
from .tool import tool_from_function
from .types import ClientOptions, ToolSpec


class Agent:
    """A reusable agent definition.

    ```python
    agent = Agent(
        name="History tutor",
        instructions="Answer history questions clearly and concisely.",
        model="gpt-4o-mini",
    )
    ```
    """

    def __init__(
        self,
        name: str,
        instructions: Optional[str] = None,
        model: Optional[str] = None,
        tools: Optional[Sequence[Any]] = None,
        handoffs: Optional[Sequence["Agent"]] = None,
        handoff_description: Optional[str] = None,
        id: Optional[str] = None,
        client: Optional[ClientOptions] = None,
        memory_backend: Union[str, MemoryBackend, None] = None,
        memo_db: Optional[str] = None,
    ) -> None:
        if not name or not name.strip():
            raise ValueError("Agent requires a `name`")
        self.name = name
        self.instructions = instructions
        self.model = model
        self.tools = [self._coerce_tool(t) for t in (tools or [])]
        self.handoffs = list(handoffs or [])
        self.handoff_description = handoff_description
        self.id = id
        self.client = client or ClientOptions()
        self.memory_backend = (
            MemoryBackend.parse(memory_backend) if memory_backend is not None
            else MemoryBackend.CLOUD
        )
        self.memo_db = memo_db

    @staticmethod
    def _coerce_tool(t: Any) -> ToolSpec:
        if isinstance(t, ToolSpec):
            return t
        if callable(t):
            return tool_from_function(t)
        raise TypeError(f"unsupported tool: {t!r}")

    def clone(self, **overrides: Any) -> "Agent":
        """Return a copy with overridden fields (used by handoffs)."""
        params: dict[str, Any] = dict(
            name=self.name,
            instructions=self.instructions,
            model=self.model,
            tools=self.tools,
            handoffs=self.handoffs,
            handoff_description=self.handoff_description,
            id=self.id,
            client=self.client,
            memory_backend=self.memory_backend,
            memo_db=self.memo_db,
        )
        params.update(overrides)
        return Agent(**params)

    def tool_schemas(self) -> list[dict[str, Any]]:
        """JSON-schema tool declarations forwarded to the cloud."""
        return [t.schema() for t in self.tools]

    def __repr__(self) -> str:  # pragma: no cover - debugging aid
        return f"Agent(name={self.name!r}, model={self.model!r})"
