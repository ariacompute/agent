"""``function_tool`` — mirrors ``@function_tool`` from the OpenAI Agents SDK.

The cloud executes tools inside its own sandbox, so only the schema
(``name`` / ``description`` / ``parameters``) is forwarded to the server. The
Python callable is kept for API compatibility and local runners; it is never
invoked by the cloud transport.
"""

from __future__ import annotations

import inspect
from typing import Any, Callable, Optional, get_type_hints

from .types import ToolSpec

__all__ = ["function_tool", "tool_from_function"]


def _schema_from_annotations(fn: Callable[..., Any]) -> dict[str, Any]:
    """Build a permissive JSON schema from the function signature."""
    try:
        hints = get_type_hints(fn)
    except Exception:  # pragma: no cover - exotic annotations
        hints = {}
    properties: dict[str, Any] = {}
    required: list[str] = []
    for name, param in inspect.signature(fn).parameters.items():
        annotation = hints.get(name)
        properties[name] = _json_type(annotation)
        if param.default is inspect.Parameter.empty:
            required.append(name)
    schema: dict[str, Any] = {
        "type": "object",
        "properties": properties,
    }
    if required:
        schema["required"] = required
    return schema


def _json_type(annotation: Any) -> dict[str, Any]:
    mapping = {str: "string", int: "integer", float: "number", bool: "boolean"}
    if annotation in mapping:
        return {"type": mapping[annotation]}
    if annotation in (list, tuple):
        return {"type": "array", "items": {}}
    if annotation is dict:
        return {"type": "object"}
    return {}


def tool_from_function(
    fn: Callable[..., Any],
    name: Optional[str] = None,
    description: Optional[str] = None,
) -> ToolSpec:
    """Wrap a Python function as a :class:`ToolSpec`."""
    return ToolSpec(
        name=name or fn.__name__,
        description=description or (inspect.getdoc(fn) or ""),
        parameters=_schema_from_annotations(fn),
        execute=fn,
    )


def function_tool(
    fn: Optional[Callable[..., Any]] = None,
    *,
    name: Optional[str] = None,
    description: Optional[str] = None,
) -> Any:
    """Decorator form of :func:`tool_from_function`.

    ```python
    @function_tool
    def history_fun_fact() -> str:
        \"\"\"Return a short history fact.\"\"\"
        return "Sharks are older than trees."
    ```
    """
    if fn is None:
        def decorator(inner: Callable[..., Any]) -> ToolSpec:
            return tool_from_function(inner, name=name, description=description)

        return decorator
    return tool_from_function(fn, name=name, description=description)
