import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { GenerateOptions, StreamChunk } from "../../../stubs/dsh-llm.ts";
import { apply, AriaAdapter, toDshChunks } from "../src/index.ts";
import type { EngineStreamEvent } from "../../../../packages/aria-bridge/src/index.ts";

async function* events(list: EngineStreamEvent[]): AsyncGenerator<EngineStreamEvent> {
  for (const e of list) {
    yield e;
  }
}

describe("toDshChunks", () => {
  it("emits usage before finish and nothing after", async () => {
    const chunks: StreamChunk[] = [];
    for await (const c of toDshChunks(
      events([
        { type: "text", text: "hi" },
        { type: "usage", usage: { inputTokens: 2, outputTokens: 1 } },
        { type: "finish", reason: "stop" },
      ]),
    )) {
      chunks.push(c);
    }
    const types = chunks.map((c) => c.type);
    assert.deepEqual(types, ["block-start", "text-delta", "block-end", "usage", "finish"]);
    assert.equal(types.indexOf("usage") < types.indexOf("finish"), true);
    assert.equal(types.at(-1), "finish");
  });
});

describe("AriaAdapter / apply", () => {
  it("registers provider route aria", () => {
    const registered: Array<{ routes: string[]; adapter: AriaAdapter }> = [];
    apply(
      {
        llm: {
          registerAdapter(routes, adapter) {
            registered.push({ routes, adapter: adapter as AriaAdapter });
            return () => undefined;
          },
        },
        tools: { register() {} },
        on() {},
      },
      { baseUrl: "http://127.0.0.1:8080/v1" },
    );
    assert.deepEqual(registered[0]?.routes, ["aria"]);
  });

  it("streams OpenAI SSE into dsh chunks", async () => {
    const fetchImpl: typeof fetch = async () =>
      new Response(
        'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":null}]}\n\ndata: [DONE]\n\n',
        { status: 200 },
      );
    const adapter = new AriaAdapter({
      engineUrl: "http://127.0.0.1:8080/v1",
      fetchImpl,
    });
    const options: GenerateOptions = {
      provider: "aria",
      model: "tiny",
      messages: [{ role: "user", content: [{ type: "text", text: "hi" }] }],
    };
    const types: string[] = [];
    for await (const c of adapter.stream(options)) {
      types.push(c.type);
    }
    assert.equal(types.at(-1), "finish");
    assert.ok(types.includes("text-delta"));
  });

  it("rejects stop sequences", async () => {
    const adapter = new AriaAdapter({ engineUrl: "http://127.0.0.1:8080/v1" });
    await assert.rejects(async () => {
      for await (const _ of adapter.stream({
        provider: "aria",
        model: "tiny",
        messages: [],
        stop: ["\n"],
      })) {
        /* drain */
      }
    });
  });
});
