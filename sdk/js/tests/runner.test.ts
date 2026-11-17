import { describe, expect, test } from "bun:test";
import { Agent, run, runStreamed } from "../src/index.js";
import type { ClientOptions } from "../src/types.js";
import { AriaError } from "../src/types.js";

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as any;
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

function fakeFetch(handler: (url: string, init: any) => any) {
  const calls: { url: string; init: any }[] = [];
  const impl = (async (url: string, init: any) => {
    calls.push({ url, init });
    return handler(url, init);
  }) as unknown as typeof globalThis.fetch;
  return { impl, calls };
}

const client: ClientOptions = { baseUrl: "http://aria.test", apiKey: "aria-test" };

describe("run input handling", () => {
  test("joins array history with newlines", async () => {
    const { impl, calls } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return jsonResponse({ id: "turn_1", output: "ok" });
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    await run(agent, [
      { role: "user", content: "a" },
      { role: "assistant", content: "b" },
      "c",
    ]);
    expect(JSON.parse(calls[1].init.body)).toEqual({ input: "a\nb\nc" });
  });

  test("rejects maxTurns < 1 before any request", async () => {
    const { impl } = fakeFetch(() => jsonResponse({ id: "turn_1", output: "ok" }));
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    await expect(run(agent, "hi", { maxTurns: 0 })).rejects.toMatchObject({
      kind: "config",
    });
  });
});

describe("runStreamed maxTurns", () => {
  test("stops after maxTurns created frames and errors", async () => {
    const frames = [
      { type: "agent.turn.created", turn_id: "t1", sequence_number: 1 },
      { type: "agent.turn.output_text.done", text: "first", sequence_number: 2 },
      { type: "agent.turn.completed", turn: { id: "t1", output: "first" }, sequence_number: 3 },
      { type: "agent.turn.created", turn_id: "t2", sequence_number: 4 },
      { type: "agent.turn.output_text.done", text: "second", sequence_number: 5 },
      { type: "agent.turn.completed", turn: { id: "t2", output: "second" }, sequence_number: 6 },
    ];
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse(frames);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "go", { maxTurns: 1 });
    await expect(streamed.completed).rejects.toMatchObject({
      kind: "config",
    });
  });

  test("empty stream resolves with empty output", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse([]);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "go");
    const result = await streamed.completed;
    expect(result.finalOutput).toBe("");
    expect(result.sessionId).toBe("sess_1");
  });

  test("failed frame surfaces a typed api error", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse([
        { type: "agent.turn.failed", error: { message: "boom" }, sequence_number: 1 },
      ]);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "go");
    const err = await streamed.completed.catch((e) => e);
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).kind).toBe("api");
  });
});

describe("approval and usage frames", () => {
  const frames = [
    { type: "agent.turn.created", turn_id: "turn_1", sequence_number: 1 },
    {
      type: "agent.turn.approval.requested",
      approval_id: "call_1",
      tool: "shell",
      sequence_number: 2,
    },
    {
      type: "agent.turn.approval.resolved",
      approval_id: "call_1",
      tool: "shell",
      decision: "unavailable",
      reason: "no approval provider configured",
      sequence_number: 3,
    },
    {
      type: "agent.turn.usage",
      usage: { input_tokens: 12, output_tokens: 5, total_tokens: 17 },
      model: "gpt-4o-mini",
      sequence_number: 4,
    },
    { type: "agent.turn.output_text.done", text: "denied", sequence_number: 5 },
    { type: "agent.turn.completed", turn: { output: "denied" }, sequence_number: 6 },
  ];

  test("decodes approval and usage events", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse(frames);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const streamed = await runStreamed(agent, "go");
    const seen: string[] = [];
    let decision: string | undefined;
    for await (const ev of streamed.events) {
      seen.push(ev.type);
      if (ev.type === "agent.turn.approval.resolved") decision = ev.decision;
    }
    expect(seen).toContain("agent.turn.approval.requested");
    expect(decision).toBe("unavailable");
    const result = await streamed.completed;
    expect(result.usage).toEqual({
      inputTokens: 12,
      outputTokens: 5,
      totalTokens: 17,
      model: "gpt-4o-mini",
    });
  });

  test("a stream without usage reports none", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_1" });
      return sseResponse([
        { type: "agent.turn.output_text.done", text: "hi", sequence_number: 1 },
      ]);
    });
    const agent = new Agent({ name: "tutor", client: { ...client, fetch: impl } });
    const result = await (await runStreamed(agent, "go")).completed;
    expect(result.usage).toBeUndefined();
  });
});
