import { describe, expect, test } from "bun:test";
import { Agent, Session, run, runStreamed, tool } from "../src/index.js";
import { decodeEvent, readSse } from "../src/index.js";
import type { ClientOptions } from "../src/types.js";

/** Minimal fake `fetch` that records requests and replays canned responses. */
function fakeFetch(handler: (url: string, init: any) => any) {
  const calls: { url: string; init: any }[] = [];
  const impl = (async (url: string, init: any) => {
    calls.push({ url, init });
    return handler(url, init);
  }) as unknown as typeof globalThis.fetch;
  return { impl, calls };
}

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  };
}

function sseResponse(frames: unknown[]) {
  const text = frames.map((f) => `data: ${JSON.stringify(f)}\n\n`).join("");
  const encoder = new TextEncoder();
  const stream = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(encoder.encode(text));
      controller.close();
    },
  });
  return { ok: true, status: 200, body: stream, text: async () => text };
}

const client: ClientOptions = { baseUrl: "http://aria.test", apiKey: "aria-test" };

describe("Agent", () => {
  test("requires a name", () => {
    expect(() => new Agent({ name: "" })).toThrow();
  });

  test("exposes tool schemas for the cloud", () => {
    const t = tool({ name: "history_fun_fact", description: "fact", parameters: { type: "object" } });
    const agent = new Agent({ name: "tutor", tools: [t] });
    const schemas = agent.toolSchemas();
    expect(schemas.length).toBe(1);
    expect(schemas[0].name).toBe("history_fun_fact");
    // `execute` is not forwarded to the cloud.
    expect(Object.keys(schemas[0])).not.toContain("execute");
  });

  test("Agent.create mirrors the quickstart factory", () => {
    const a = Agent.create({ name: "History tutor", instructions: "be brief" });
    expect(a.name).toBe("History tutor");
    expect(a.instructions).toBe("be brief");
  });
});

describe("run", () => {
  test("creates a session, posts the input and returns finalOutput", async () => {
    const { impl, calls } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      if (url.endsWith("/events")) return jsonResponse({ id: "turn_1", output: "476 AD" });
      return jsonResponse({ error: "unexpected" }, 500);
    });
    const agent = new Agent({ name: "History tutor", client: { ...client, fetch: impl } });
    const result = await run(agent, "When did the Roman Empire fall?");
    expect(result.finalOutput).toBe("476 AD");
    expect(result.sessionId).toBe("sess_1");
    expect(result.turnId).toBe("turn_1");
    expect(result.lastAgent?.name).toBe("History tutor");
    expect(result.history).toEqual([
      "When did the Roman Empire fall?",
      { role: "assistant", content: "476 AD" },
    ]);
    // Session creation references the agent by name and sends the beta header.
    const create = calls[0];
    expect(create.url).toBe("http://aria.test/v1/agents/sessions");
    expect(JSON.parse(create.init.body)).toEqual({ agent: "History tutor" });
    expect(create.init.headers["OpenAI-Beta"]).toBe("agents=v1");
    expect(create.init.headers["Authorization"]).toBe("Bearer aria-test");
    // The turn is posted to the session's events endpoint.
    expect(calls[1].url).toBe("http://aria.test/v1/agents/sessions/sess_1/events");
    expect(JSON.parse(calls[1].init.body)).toEqual({ input: "When did the Roman Empire fall?" });
  });

  test("reuses a supplied session so the conversation continues", async () => {
    const { impl, calls } = fakeFetch((url: string) => {
      if (url.endsWith("/events")) return jsonResponse({ id: "turn_2", output: "second" });
      return jsonResponse({ error: "unexpected" }, 500);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const result = await run(agent, "again", { session: "sess_existing" });
    expect(result.sessionId).toBe("sess_existing");
    expect(calls.length).toBe(1);
  });

  test("surfaces HTTP errors instead of silently succeeding", async () => {
    const { impl } = fakeFetch(() => jsonResponse({ error: "not found" }, 404));
    const agent = new Agent({ name: "ghost", client: { ...client, fetch: impl } });
    await expect(run(agent, "hi")).rejects.toThrow(/404/);
  });
});

describe("runStreamed", () => {
  const frames = [
    { type: "agent.turn.created", turn_id: "turn_1", session_id: "sess_1", sequence_number: 1 },
    { type: "agent.turn.item.added", item: { id: "msg_1", type: "message" }, sequence_number: 2 },
    { type: "agent.turn.output_text.delta", delta: "4", sequence_number: 3 },
    { type: "agent.turn.output_text.delta", delta: "76", sequence_number: 4 },
    {
      type: "agent.turn.completed",
      turn: { id: "turn_1", status: "completed", output: "476" },
      sequence_number: 5,
    },
  ];

  test("streams deltas and resolves the final output", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse(frames);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "Rome?");
    const seen: string[] = [];
    let text = "";
    for await (const ev of streamed.events) {
      seen.push(ev.type);
      if (ev.type === "agent.turn.output_text.delta") text += ev.delta;
    }
    expect(seen[0]).toBe("agent.turn.created");
    expect(text).toBe("476");
    const result = await streamed.completed;
    expect(result.finalOutput).toBe("476");
    expect(result.turnId).toBe("turn_1");
  });

  test("rejects when the turn fails", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse([
        { type: "agent.turn.failed", error: { message: "boom" }, sequence_number: 1 },
      ]);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "Rome?");
    await expect(streamed.completed).rejects.toThrow("boom");
  });
});

describe("SSE decoding", () => {
  test("parses frames and ignores keep-alives and [DONE]", async () => {
    const raw =
      ": keep-alive\n\n" +
      'data: {"type":"agent.turn.output_text.delta","delta":"a"}\n\n' +
      "data: [DONE]\n\n" +
      'data: {"type":"agent.turn.completed"}\n\n';
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode(raw));
        c.close();
      },
    });
    const out: string[] = [];
    for await (const f of readSse(stream as unknown as AsyncIterable<Uint8Array>)) {
      out.push(String(f.type));
    }
    expect(out).toEqual(["agent.turn.output_text.delta", "agent.turn.completed"]);
  });

  test("decodeEvent narrows known types", () => {
    const ev = decodeEvent({ type: "agent.turn.output_text.delta", delta: "hi" });
    expect(ev.type).toBe("agent.turn.output_text.delta");
    expect((ev as { delta: string }).delta).toBe("hi");
    const other = decodeEvent({ type: "agent.output.command_execution_output.delta", delta: "x" });
    expect(other.type).toBe("agent.output.command_execution_output.delta");
  });
});

describe("Session long-term memory", () => {
  test("memorize posts the key/value to the session memory endpoint", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse({ key: "user_name", value: "Ada" }));
    const session = new Session("sess_1", { ...client, fetch: impl });
    await session.memorize("user_name", "Ada");
    expect(calls[0].url).toBe("http://aria.test/v1/agents/sessions/sess_1/memory");
    expect(calls[0].init.method).toBe("POST");
    expect(JSON.parse(calls[0].init.body)).toEqual({
      key: "user_name",
      value: "Ada",
      kind: "long_term",
    });
  });

  test("recall reads the value and returns null when missing", async () => {
    const { impl } = fakeFetch(() => jsonResponse({ key: "user_name" }));
    const session = new Session("sess_1", { ...client, fetch: impl });
    expect(await session.recall("user_name")).toBeNull();
  });

  test("recall returns the stored value", async () => {
    const { impl } = fakeFetch(() => jsonResponse({ key: "user_name", value: "Ada" }));
    const session = new Session("sess_1", { ...client, fetch: impl });
    expect(await session.recall("user_name")).toBe("Ada");
  });
});

describe("Session", () => {
  test("create posts the agent reference and instructions", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse({ id: "sess_9" }));
    const agent = new Agent({
      name: "tutor",
      instructions: "be brief",
      client: { ...client, fetch: impl },
    });
    const session = await Session.create(agent);
    expect(session.id).toBe("sess_9");
    expect(JSON.parse(calls[0].init.body)).toEqual({
      agent: "tutor",
      instructions: "be brief",
    });
  });
});
