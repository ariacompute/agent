"""Shared types for :mod:`ariacompute_agent`.

Names mirror the OpenAI Agents SDK (``openai-agents``) so the official
quickstart transfers unchanged: ``Agent``, ``Runner.run``,
``Runner.run_streamed``, ``function_tool``, ``result.final_output``,
``result.to_input_list()``.
"""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, AsyncIterator, Callable, Iterable, Optional, Sequence, Union

Role = str

@dataclass
class HistoryItem:
    """One entry of the conversation history."""

    role: Role
    content: str

    def to_input(self) -> dict[str, str]:
        return {"role": self.role, "content": self.content}


HistoryInput = Union[str, HistoryItem, Sequence[Union[str, HistoryItem]]]


@dataclass
class ToolSpec:
    """A tool declaration forwarded to the cloud agent."""

    name: str
    description: str = ""
    parameters: dict[str, Any] = field(default_factory=lambda: {"type": "object", "properties": {}})
    execute: Optional[Callable[..., Any]] = None

    def schema(self) -> dict[str, Any]:
        """The JSON-schema part sent to the server (never ``execute``)."""
        return {
            "name": self.name,
            "description": self.description,
            "parameters": self.parameters,
        }


@dataclass
class ClientOptions:
    """Connection settings for the aria-agent-cloud service."""

    base_url: Optional[str] = None
    api_key: Optional[str] = None
    beta_header: str = "agents=v1"


@dataclass
class RunResult:
    """Result of a completed run (mirrors ``RunResult`` in the Agents SDK)."""

    final_output: str
    history: list[HistoryInput]
    last_agent: Optional[Any] = None
    session_id: Optional[str] = None
    turn_id: Optional[str] = None

    def to_input_list(self) -> list[dict[str, str]]:
        """Flatten the history into OpenAI-style message dicts."""
        out: list[dict[str, str]] = []
        for item in self.history:
            if isinstance(item, str):
                out.append({"role": "user", "content": item})
            elif isinstance(item, HistoryItem):
                out.append(item.to_input())
            elif isinstance(item, (list, tuple)):
                for sub in item:
                    if isinstance(sub, str):
                        out.append({"role": "user", "content": sub})
                    elif isinstance(sub, HistoryItem):
                        out.append(sub.to_input())
        return out


@dataclass
class StreamedRunResult:
    """Handle returned by :meth:`Runner.run_streamed`."""

    events: AsyncIterator[dict[str, Any]]
    completed: Any  # Awaitable[RunResult]
