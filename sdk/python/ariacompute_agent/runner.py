"""``Runner`` — mirrors ``Runner.run`` / ``Runner.run_streamed`` from the Agents SDK.

Both drive one turn against the cloud's beta Agents API:
``POST /v1/agents/sessions/{id}/events`` (blocking) or
``POST /v1/agents/sessions/{id}/events/stream`` (SSE).
"""

from __future__ import annotations

import asyncio
from typing import Any, AsyncIterator, Optional, Union

from .session import Session
from .transport import async_stream_json, post_json, resolve_client
from .types import ClientOptions, HistoryInput, HistoryItem, RunResult, StreamedRunResult


def _to_text(value: HistoryInput) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, HistoryItem):
        return value.content
    if isinstance(value, dict):  # {"role": ..., "content": ...}
        return str(value.get("content", ""))
    parts: list[str] = []
    for item in value or []:
        if isinstance(item, str):
            parts.append(item)
        elif isinstance(item, HistoryItem):
            parts.append(item.content)
        elif isinstance(item, dict):
            parts.append(str(item.get("content", "")))
    return "\n".join(parts)


def _ensure_session(agent: Any, client: ClientOptions, session: Any) -> Session:
    if isinstance(session, Session):
        return session
    if isinstance(session, str) and session:
        return Session(session, client)
    return Session.create(agent, client)


class Runner:
    """Runs agents against the aria-agent-cloud service."""

    @staticmethod
    async def run(
        agent: Any,
        input: HistoryInput,
        *,
        session: Optional[Union[Session, str]] = None,
        client: Optional[ClientOptions] = None,
    ) -> RunResult:
        """Run one turn to completion and return the result.

        ```python
        result = await Runner.run(agent, "When did the Roman Empire fall?")
        print(result.final_output)
        ```
        """
        merged = client or agent.client
        resolved = resolve_client(merged)
        sess = _ensure_session(agent, merged, session)
        text = _to_text(input)

        turn = await asyncio.to_thread(
            post_json,
            resolved,
            f"/v1/agents/sessions/{sess.id}/events",
            {"input": text},
        )
        output = turn.get("output") or ""
        sess.record(input, output)
        return RunResult(
            final_output=output,
            history=list(sess.history),
            last_agent=agent,
            session_id=sess.id,
            turn_id=turn.get("id"),
        )

    @staticmethod
    async def run_streamed(
        agent: Any,
        input: HistoryInput,
        *,
        session: Optional[Union[Session, str]] = None,
        client: Optional[ClientOptions] = None,
    ) -> StreamedRunResult:
        """Run one turn, streaming events (``agent.turn.*`` frames).

        ```python
        streamed = await Runner.run_streamed(agent, "Tell me something surprising")
        async for ev in streamed.events:
            if ev["type"] == "agent.turn.output_text.delta":
                print(ev["delta"], end="")
        result = await streamed.completed
        print(result.final_output)
        ```
        """
        merged = client or agent.client
        resolved = resolve_client(merged)
        sess = _ensure_session(agent, merged, session)
        text = _to_text(input)

        state: dict[str, Any] = {"text": "", "turn_id": None, "error": None}
        buffered: list[dict[str, Any]] = []
        drained = asyncio.Event()

        def _note(frame: dict[str, Any]) -> None:
            state["turn_id"] = frame.get("turn_id") or state["turn_id"]
            kind = frame.get("type")
            if kind == "agent.turn.output_text.delta":
                state["text"] += str(frame.get("delta") or "")
            elif kind in ("agent.turn.output_text.done", "agent.turn.completed"):
                out = frame.get("text") or (frame.get("turn") or {}).get("output")
                if isinstance(out, str) and out:
                    state["text"] = out
            elif kind == "agent.turn.failed":
                state["error"] = (frame.get("error") or {}).get("message") or "run failed"

        async def _drain() -> None:
            async for frame in async_stream_json(
                resolved,
                f"/v1/agents/sessions/{sess.id}/events/stream",
                {"input": text},
            ):
                buffered.append(frame)
                _note(frame)
            drained.set()

        drain_task = asyncio.ensure_future(_drain())

        async def events() -> AsyncIterator[dict[str, Any]]:
            await asyncio.shield(drain_task)
            for frame in buffered:
                yield frame

        async def completed() -> RunResult:
            await asyncio.shield(drain_task)
            if state["error"]:
                raise RuntimeError(state["error"])
            sess.record(input, state["text"])
            return RunResult(
                final_output=state["text"],
                history=list(sess.history),
                last_agent=agent,
                session_id=sess.id,
                turn_id=state["turn_id"],
            )

        return StreamedRunResult(events=events(), completed=completed())
