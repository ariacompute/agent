"""``ariacompute-agent`` — the Aria agent SDK.

The API mirrors the OpenAI Agents SDK (``openai-agents``) so the official
quickstart transfers unchanged:

```python
import asyncio

from ariacompute_agent import Agent, Runner


async def main() -> None:
    agent = Agent(
        name="History tutor",
        instructions="Answer history questions clearly and concisely.",
        model="gpt-4o-mini",
    )
    result = await Runner.run(agent, "When did the Roman Empire fall?")
    print(result.final_output)


asyncio.run(main())
```
"""

from .agent import Agent
from .runner import Runner
from .session import Session
from .tool import function_tool, tool_from_function
from .transport import (
    DEFAULT_BETA_HEADER,
    AgentTransportError,
    get_json,
    post_json,
    resolve_client,
)
from .types import (
    ClientOptions,
    HistoryInput,
    HistoryItem,
    RunResult,
    StreamedRunResult,
    ToolSpec,
)

__all__ = [
    "Agent",
    "Runner",
    "Session",
    "function_tool",
    "tool_from_function",
    "ClientOptions",
    "HistoryItem",
    "HistoryInput",
    "RunResult",
    "StreamedRunResult",
    "ToolSpec",
    "AgentTransportError",
    "resolve_client",
    "post_json",
    "get_json",
    "DEFAULT_BETA_HEADER",
]

__version__ = "0.1.0"
