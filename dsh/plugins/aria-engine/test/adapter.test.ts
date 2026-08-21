import assert from "node:assert/strict";
import { describe, it } from "node:test";
import type { GenerateOptions, StreamChunk } from "../../../stubs/dsh-llm.ts";
import { apply, AriaAdapter, toDshChunks } from "../src/index.ts";
import { openaiMessagesFrom, serializeTools } from "../src/adapter.ts";
import type { EngineFactory, EngineLike } from "../src/engine-binding.ts";
import type { EngineStreamEvent } from "../src/engine.ts";

function fakeFactory(result: unknown): { factory: EngineFactory; last: { options?: unknown; tools?: unknown } } {
  const last: { options?: unknown; tools?: unknown } = {};
  const engine: EngineLike = {
    complete(_messages, options, tools) {
      last.options = options;
      last.tools = tools;
      return result;
    },
  };
  return { factory: () => engine, last };
}

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

  it("maps wire tool-call indices to distinct block indices when text precedes", async () => {
    const chunks: StreamChunk[] = [];
    for await (const c of toDshChunks(
      events([
        { type: "text", text: "ok" },
        { type: "tool-call", index: 0, id: "call_a", name: "sandbox_exec", arguments: '{"cmd":"ls"}' },
        { type: "finish", reason: "tool_calls" },
      ]),
    )) {
      chunks.push(c);
    }
    const types = chunks.map((c) => c.type);
    assert.deepEqual(types, [
      "block-start",
      "text-delta",
      "block-start",
      "tool-call-delta",
      "block-end",
      "block-end",
      "finish",
    ]);
    const toolStart = chunks.find((c) => c.type === "block-start" && c.blockType === "tool-call");
    assert.equal(toolStart?.type === "block-start" ? toolStart.index : -1, 1);
    const toolDelta = chunks.find((c) => c.type === "tool-call-delta");
    assert.deepEqual(
      toolDelta && toolDelta.type === "tool-call-delta"
        ? {
            index: toolDelta.index,
            id: toolDelta.id,
            name: toolDelta.name,
            argumentsDelta: toolDelta.argumentsDelta,
          }
        : null,
      { index: 1, id: "call_a", name: "sandbox_exec", argumentsDelta: '{"cmd":"ls"}' },
    );
    const blocks = chunks.filter((c) => c.type === "block-end").map((c) => c.block);
    assert.deepEqual(blocks, [
      { type: "text", text: "ok" },
      { type: "tool-call", id: "call_a", name: "sandbox_exec", arguments: '{"cmd":"ls"}' },
    ]);
  });

  it("emits tool-call block with canonical id when only tool calls are present", async () => {
    const chunks: StreamChunk[] = [];
    for await (const c of toDshChunks(
      events([
        { type: "tool-call", index: 0, id: "call_1", name: "memo_add", arguments: '{"text":"x"}' },
        { type: "finish", reason: "tool_calls" },
      ]),
    )) {
      chunks.push(c);
    }
    const start = chunks[0];
    assert.equal(start?.type, "block-start");
    assert.equal(start.type === "block-start" ? start.index : -1, 0);
    const end = chunks.find((c) => c.type === "block-end");
    assert.deepEqual(
      end && end.type === "block-end" ? end.block : null,
      { type: "tool-call", id: "call_1", name: "memo_add", arguments: '{"text":"x"}' },
    );
    const finish = chunks.at(-1);
    assert.equal(finish?.type, "finish");
    assert.equal(finish.type === "finish" ? finish.reason.kind : "", "tool-calls");
  });
});

describe("openaiMessagesFrom / serializeTools", () => {
  it("serializes tool-call history and tool results", () => {
    const messages = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      messages: [
        { role: "user", content: [{ type: "text", text: "run ls" }] },
        {
          role: "assistant",
          content: [
            { type: "tool-call", id: "call_a", name: "sandbox_exec", arguments: { cmd: "ls" } },
          ],
        },
        {
          role: "user",
          content: [{ type: "tool-result", toolCallId: "call_a", content: "bin  src" }],
        },
      ],
    });
    assert.deepEqual(messages, [
      { role: "user", content: "run ls" },
      {
        role: "assistant",
        tool_calls: [
          {
            id: "call_a",
            type: "function",
            function: { name: "sandbox_exec", arguments: '{"cmd":"ls"}' },
          },
        ],
      },
      { role: "tool", tool_call_id: "call_a", content: "bin  src" },
    ]);
  });

  it("marks failed tool results with is_error and falls back to callId", () => {
    const messages = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      messages: [
        {
          role: "assistant",
          content: [{ type: "tool-call", callId: "c_fb", name: "sandbox_exec", arguments: "{}" }],
        },
        {
          role: "user",
          content: [
            {
              type: "tool-result",
              toolCallId: "c_fb",
              content: [{ type: "text", text: "boom" }],
              isError: true,
            },
          ],
        },
      ],
    });
    assert.deepEqual(messages, [
      {
        role: "assistant",
        tool_calls: [
          { id: "c_fb", type: "function", function: { name: "sandbox_exec", arguments: "{}" } },
        ],
      },
      { role: "tool", tool_call_id: "c_fb", content: "boom", is_error: true },
    ]);
  });

  it("serializes dsh tools to OpenAI function tools", () => {
    const tools = serializeTools([
      {
        name: "sandbox_exec",
        description: "run command",
        parameters: { type: "object", properties: { cmd: { type: "string" } } },
      },
      { name: "memo_add", description: "add memory" },
    ]);
    assert.deepEqual(tools, [
      {
        type: "function",
        function: {
          name: "sandbox_exec",
          description: "run command",
          parameters: { type: "object", properties: { cmd: { type: "string" } } },
        },
      },
      { type: "function", function: { name: "memo_add", description: "add memory" } },
    ]);
    assert.equal(serializeTools(undefined), undefined);
    assert.equal(serializeTools([]), undefined);
  });
});

describe("AriaAdapter / apply", () => {
  it("registers provider route aria from cordis config.bundle", () => {
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
      { bundle: "/my.bundle", ffiLib: "/my/lib.so", model: "aria-tiny" },
    );
    assert.deepEqual(registered[0]?.routes, ["aria"]);
    assert.ok(registered[0]?.adapter instanceof AriaAdapter);
  });

  it("throws when no bundle is configured", () => {
    assert.throws(() =>
      apply(
        {
          llm: { registerAdapter() { return () => undefined; } },
          tools: { register() {} },
          on() {},
        },
        {},
      ),
    );
  });

  it("streams engine result into dsh chunks via in-process engine", async () => {
    const { factory, last } = fakeFactory({
      choices: [{ message: { content: "ok" }, finish_reason: "stop" }],
    });
    const adapter = new AriaAdapter({
      bundlePath: "/my.bundle",
      ffiLib: "/my/lib.so",
      defaultModel: "aria-tiny",
      engineFactory: factory,
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
    assert.deepEqual(last.options, { model: "tiny", temperature: undefined, max_tokens: undefined });
  });

  it("rejects stop sequences", async () => {
    const adapter = new AriaAdapter({
      bundlePath: "/my.bundle",
      defaultModel: "aria-tiny",
      engineFactory: fakeFactory({ choices: [{ message: { content: "" } }] }).factory,
    });
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

  it("requires a model (config.defaultModel or request model)", async () => {
    const adapter = new AriaAdapter({
      bundlePath: "/my.bundle",
      engineFactory: fakeFactory({ choices: [{ message: { content: "" } }] }).factory,
    });
    await assert.rejects(async () => {
      for await (const _ of adapter.stream({
        provider: "aria",
        messages: [{ role: "user", content: "hi" }],
      })) {
        /* drain */
      }
    });
  });

  it("streams tool-call engine result into dsh tool-call blocks with canonical id", async () => {
    const { factory } = fakeFactory({
      choices: [
        {
          message: {
            content: "running",
            tool_calls: [
              { id: "call_1", function: { name: "sandbox_exec", arguments: '{"cmd":"ls"}' } },
            ],
          },
          finish_reason: "tool_calls",
        },
      ],
    });
    const adapter = new AriaAdapter({
      bundlePath: "/my.bundle",
      ffiLib: "/my/lib.so",
      defaultModel: "aria-tiny",
      engineFactory: factory,
    });
    const chunks: StreamChunk[] = [];
    for await (const c of adapter.stream({
      provider: "aria",
      model: "tiny",
      messages: [{ role: "user", content: "run ls" }],
      tools: [{ name: "sandbox_exec", description: "run", parameters: { type: "object" } }],
    })) {
      chunks.push(c);
    }
    const toolEnd = chunks.find(
      (c) => c.type === "block-end" && c.block.type === "tool-call",
    );
    assert.deepEqual(
      toolEnd && toolEnd.type === "block-end" && toolEnd.block.type === "tool-call"
        ? toolEnd.block
        : null,
      { type: "tool-call", id: "call_1", name: "sandbox_exec", arguments: '{"cmd":"ls"}' },
    );
    const finish = chunks.at(-1);
    assert.equal(finish?.type, "finish");
    assert.equal(finish.type === "finish" ? finish.reason.kind : "", "tool-calls");
  });
});
