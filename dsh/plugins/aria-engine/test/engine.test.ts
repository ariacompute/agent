import { describe, it, expect } from "bun:test";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { openaiMessagesFrom, parseEngineResult, serializeTools } from "../src/engine.ts";

describe("parseEngineResult", () => {
  it("emits text + usage + finish for a text choice", () => {
    const json = {
      choices: [{ message: { role: "assistant", content: "hi", tool_calls: [] } }],
      usage: { prompt_tokens: 1, completion_tokens: 2, total_tokens: 3 },
      finish_reason: "stop",
    };
    const events = parseEngineResult(json);
    // text, then usage, then finish
    expect(events.map((e) => e.type)).toEqual(["text", "usage", "finish"]);
    const text = events[0];
    expect(text?.type === "text" ? text.text : "").toBe("hi");
    const usage = events.find((e) => e.type === "usage");
    expect(usage?.type === "usage" ? usage.usage : null).toEqual({ inputTokens: 1, outputTokens: 2 });
    const finish = events.at(-1);
    expect(finish?.type).toBe("finish");
    expect(finish?.type === "finish" ? finish.reason : "").toBe("stop");
  });

  it("maps tool_calls into our ToolCall shape (text skipped when empty)", () => {
    const json = {
      choices: [
        {
          message: {
            content: "",
            tool_calls: [
              { id: "call_1", function: { name: "f", arguments: '{"x":1}' } },
            ],
          },
        },
      ],
      usage: { prompt_tokens: 0, completion_tokens: 0, total_tokens: 0 },
      finish_reason: "tool_calls",
    };
    const events = parseEngineResult(json);
    // tool-call, usage, finish (no text event because content is empty)
    expect(events.map((e) => e.type)).toEqual(["tool-call", "usage", "finish"]);
    const tc = events.find((e) => e.type === "tool-call");
    expect(
      tc && tc.type === "tool-call" ? { id: tc.id, name: tc.name, arguments: tc.arguments, index: tc.index } : null,
    ).toEqual({ id: "call_1", name: "f", arguments: '{"x":1}', index: 0 });
  });

  it("does not throw on a malformed payload; emits a finish", () => {
    const events = parseEngineResult({} as never);
    expect(events.length).toBeGreaterThan(0);
    expect(events.at(-1)?.type).toBe("finish");
  });
});

describe("openaiMessagesFrom", () => {
  it("converts a single text message", () => {
    const out = openaiMessagesFrom({ provider: "aria", model: "tiny", messages: [{ role: "user", content: "hello" }] });
    expect(out).toEqual([{ role: "user", content: "hello" }]);
  });

  it("keeps sequential assistant text turns as separate messages", () => {
    const out = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      messages: [
        { role: "assistant", content: "a" },
        { role: "assistant", content: "b" },
      ],
    });
    expect(out).toEqual([
      { role: "assistant", content: "a" },
      { role: "assistant", content: "b" },
    ]);
  });

  it("serializes a tool-result block into a tool message", () => {
    const out = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      messages: [
        {
          role: "user",
          content: [{ type: "tool-result", toolCallId: "c1", content: "r" }],
        },
      ],
    });
    expect(out).toEqual([{ role: "tool", tool_call_id: "c1", content: "r" }]);
  });

  it("serializes tool-call turns with arguments", () => {
    const out = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      messages: [
        {
          role: "assistant",
          content: [{ type: "tool-call", id: "c2", name: "g", arguments: '{"y":2}' }],
        },
      ],
    });
    expect(out).toEqual([
      {
        role: "assistant",
        tool_calls: [
          { id: "c2", type: "function", function: { name: "g", arguments: '{"y":2}' } },
        ],
      },
    ]);
  });
});

describe("serializeTools", () => {
  it("keeps name/description/parameters metadata", () => {
    const tools = [
      {
        name: "f",
        description: "does f",
        parameters: { type: "object", properties: {} },
      },
    ];
    const out = serializeTools(tools);
    expect(out).toEqual([
      { type: "function", function: { name: "f", description: "does f", parameters: { type: "object", properties: {} } } },
    ]);
  });

  it("returns undefined for empty/missing tool lists", () => {
    expect(serializeTools(undefined)).toBeUndefined();
    expect(serializeTools([])).toBeUndefined();
  });
});
