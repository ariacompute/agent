# `ariacompute-agent`

The Aria agent SDK for Python. The API mirrors the
[OpenAI Agents SDK](https://developers.openai.com/api/docs/guides/agents/quickstart)
(`Agent`, `Runner.run`, `Runner.run_streamed`, `function_tool`, `Session`) and
talks to an `aria-agent-cloud` deployment through its **beta Agents** REST API.

```bash
pip install ariacompute-agent
```

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

## Tools

```python
from ariacompute_agent import function_tool


@function_tool
def history_fun_fact() -> str:
    """Return a short history fact."""
    return "Sharks are older than trees."


agent = Agent(name="History tutor", tools=[history_fun_fact])
```

The cloud executes tools inside its own sandbox, so only the schema
(`name` / `description` / `parameters`) is sent to the server.

## Streaming

```python
streamed = await Runner.run_streamed(agent, "Tell me something surprising")
async for ev in streamed.events:
    if ev["type"] == "agent.turn.output_text.delta":
        print(ev["delta"], end="")
result = await streamed.completed
print(result.final_output)
```

Frames use the `agent.*` envelope; `agent.turn.completed` (or
`agent.turn.failed`) is the **only** terminal event — there is no
`data: [DONE]` sentinel.

## Sessions

```python
session = Session.create(agent)
await Runner.run(agent, "hello", session=session)
await Runner.run(agent, "and then?", session=session)
```

## Configuration

| Option | Environment variable | Default |
| --- | --- | --- |
| `base_url` | `ARIA_AGENT_BASE_URL` | `http://localhost:3000` |
| `api_key` | `ARIA_AGENT_API_KEY` | _(none)_ |
| `beta_header` | – | `agents=v1` (`OpenAI-Beta`) |

## Tests

```bash
python3 -m unittest discover -s tests
```
