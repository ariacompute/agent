# `@ariacompute/agent`

The Aria agent SDK for TypeScript/JavaScript. The API mirrors the
[OpenAI Agents SDK](https://developers.openai.com/api/docs/guides/agents/quickstart)
(`Agent`, `run`, `runStreamed`, `tool`, `Session`) and talks to an
`aria-agent-cloud` deployment through its **beta Agents** REST API.

```bash
npm install @ariacompute/agent
```

```ts
import { Agent, run, tool } from "@ariacompute/agent";

const historyFunFact = tool({
  name: "history_fun_fact",
  description: "Return a short history fact.",
  parameters: { type: "object", properties: {} },
});

const agent = new Agent({
  name: "History tutor",
  instructions: "Answer history questions clearly and concisely.",
  model: "gpt-4o-mini",
  tools: [historyFunFact],
});

const result = await run(agent, "When did the Roman Empire fall?");
console.log(result.finalOutput); // -> "476 AD"
```

## Streaming

```ts
const streamed = await runStreamed(agent, "Tell me something surprising");
for await (const ev of streamed.events) {
  if (ev.type === "agent.turn.output_text.delta") process.stdout.write(ev.delta);
}
const result = await streamed.completed;
console.log(result.finalOutput);
```

Frames use the `agent.*` envelope: `agent.turn.created`, `agent.turn.in_progress`,
`agent.turn.item.added`, `agent.turn.item.done`, `agent.turn.output_text.delta`,
`agent.turn.output_text.done`, `agent.turn.completed` / `agent.turn.failed`.
`agent.turn.completed` (or `agent.turn.failed`) is the **only** terminal event —
there is no `data: [DONE]` sentinel.

## Sessions

```ts
const session = await Session.create(agent);
await run(agent, "hello", { session });
await run(agent, "and then?", { session }); // continues the same conversation
```

## Configuration

| Option | Environment variable | Default |
| --- | --- | --- |
| `baseUrl` | `ARIA_AGENT_BASE_URL` | `http://localhost:3000` |
| `apiKey` | `ARIA_AGENT_API_KEY` | _(none)_ |
| `betaHeader` | – | `agents=v1` (`OpenAI-Beta`) |

## Tests

```bash
bun test          # or: npm test
npm run typecheck
```
