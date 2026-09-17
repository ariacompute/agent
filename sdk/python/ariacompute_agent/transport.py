"""HTTP transport for the aria-agent-cloud **beta Agents** API.

Only the standard library is used (``urllib``), so the package installs without
any runtime dependency and works in restricted environments.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from typing import Any, AsyncIterator, Iterator, Optional

from .types import ClientOptions

DEFAULT_BETA_HEADER = "agents=v1"


class AgentTransportError(RuntimeError):
    """Raised when the cloud returns a non-2xx response."""


def resolve_client(options: Optional[ClientOptions] = None) -> ClientOptions:
    """Fill in defaults from the environment."""
    options = options or ClientOptions()
    base_url = options.base_url or os.environ.get("ARIA_AGENT_BASE_URL") or "http://localhost:3000"
    api_key = options.api_key or os.environ.get("ARIA_AGENT_API_KEY")
    return ClientOptions(
        base_url=base_url.rstrip("/"),
        api_key=api_key,
        beta_header=options.beta_header or DEFAULT_BETA_HEADER,
    )


def _request(
    client: ClientOptions,
    method: str,
    path: str,
    body: Optional[dict[str, Any]] = None,
    stream: bool = False,
) -> urllib.response.addinfourl:
    data = json.dumps(body).encode("utf-8") if body is not None else None
    headers = {
        "OpenAI-Beta": client.beta_header,
        "Accept": "text/event-stream" if stream else "application/json",
    }
    if data is not None:
        headers["Content-Type"] = "application/json"
    if client.api_key:
        headers["Authorization"] = f"Bearer {client.api_key}"
    req = urllib.request.Request(  # noqa: S310 - base_url is operator-configured
        f"{client.base_url}{path}", data=data, headers=headers, method=method
    )
    try:
        return urllib.request.urlopen(req)  # noqa: S310 - see above
    except urllib.error.HTTPError as e:  # pragma: no cover - exercised via tests
        detail = e.read().decode("utf-8", "replace")[:400]
        raise AgentTransportError(f"aria agent request failed ({e.code}): {detail}") from e


def post_json(client: ClientOptions, path: str, body: dict[str, Any]) -> dict[str, Any]:
    """POST JSON and decode the JSON response."""
    with _request(client, "POST", path, body) as res:
        raw = res.read().decode("utf-8")
    return json.loads(raw) if raw else {}


def get_json(client: ClientOptions, path: str) -> dict[str, Any]:
    """GET and decode the JSON response."""
    with _request(client, "GET", path) as res:
        raw = res.read().decode("utf-8")
    return json.loads(raw) if raw else {}


def _iter_bytes(res: Any) -> Iterator[bytes]:
    while True:
        chunk = res.read(1024)
        if not chunk:
            break
        yield chunk


def iter_sse(res: Any) -> Iterator[dict[str, Any]]:
    """Decode an SSE body into JSON frames.

    The aria envelope has **no** ``data: [DONE]`` sentinel: the terminal frame
    is ``agent.turn.completed`` / ``agent.turn.failed``, so the stream simply
    ends when the server closes it.
    """
    buffer = ""
    for chunk in _iter_bytes(res):
        buffer += chunk.decode("utf-8", "replace")
        while "\n\n" in buffer:
            raw, buffer = buffer.split("\n\n", 1)
            for line in raw.splitlines():
                if not line.startswith("data:"):
                    continue
                data = line[5:].strip()
                if not data or data == "[DONE]":
                    continue
                try:
                    yield json.loads(data)
                except json.JSONDecodeError:
                    # Ignore keep-alives / malformed frames.
                    continue


def stream_json(
    client: ClientOptions, path: str, body: dict[str, Any]
) -> Iterator[dict[str, Any]]:
    """POST JSON and yield decoded SSE frames (blocking iterator)."""
    res = _request(client, "POST", path, body, stream=True)
    try:
        yield from iter_sse(res)
    finally:
        res.close()


async def async_stream_json(
    client: ClientOptions, path: str, body: dict[str, Any]
) -> AsyncIterator[dict[str, Any]]:
    """Async wrapper over :func:`stream_json` (the transport is blocking I/O)."""
    for frame in stream_json(client, path, body):
        yield frame
