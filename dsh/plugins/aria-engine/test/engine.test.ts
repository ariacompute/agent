import assert from "node:assert/strict";
import { describe, it } from "node:test";
import {
  openaiMessagesFrom,
  parseEngineResult,
  serializeTools,
} from "../src/engine.ts";

describe("parseEngineResult", () => {
  it("parses text content with usage and stop finish", () => {
    const events = parseEngineResult({
      choices: [{ message: { content: "hello" }, finish_reason: "stop" }],
      usage: { prompt_tokens: 1, completion_tokens: 2 },
    });
    assert.deepEqual(events, [
      { type: "text", text: "hello" },
      { type: "usage", usage: { inputTokens: 1, outputTokens: 2 } },
      { type: "finish", reason: "stop" },
    ]);
  });

  it("parses tool_calls with wire index and canonical id", () => {
    const events = parseEngineResult({
      choices: [
        {
          message: {
            content: "",
            tool_calls: [
              { id: "call_1", function: { name: "sandbox_exec", arguments: '{"cmd":"ls"}' } },
            ],
          },
          finish_reason: "tool_calls",
        },
      ],
    });
    const tc = events.find((e) => e.type === "tool-call");
    assert.deepEqual(
      tc && tc.type === "tool-call" ? { id: tc.id, name: tc.name, arguments: tc.arguments, index: tc.index } : null,
      { id: "call_1", name: "sandbox_exec", arguments: '{"cmd":"ls"}', index: 0 },
    );
    assert.equal(events.at(-1)?.type, "finish");
    const finish1 = events.at(-1);
    assert.equal(finish1?.type === "finish" ? finish1.reason : "", "tool_calls");
  });

  it("maps length finish_reason", () => {
    const events = parseEngineResult({
      choices: [{ message: { content: "trunc" }, finish_reason: "length" }],
    });
    assert.equal(events.at(-1)?.type, "finish");
    const finish2 = events.at(-1);
    assert.equal(finish2?.type === "finish" ? finish2.reason : "", "length");
  });

  it("falls back to call index when id is missing", () => {
    const events = parseEngineResult({
      choices: [{ message: { tool_calls: [{ function: { name: "memo_add" } }] } }],
    });
    const tc = events.find((e) => e.type === "tool-call");
    assert.equal(tc?.type === "tool-call" ? tc.id : "", "call_0");
  });
});

describe("openaiMessagesFrom", () => {
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

  it("drops non-object messages and keeps system prompt", () => {
    const messages = openaiMessagesFrom({
      provider: "aria",
      model: "tiny",
      system: "be brief",
      messages: [
        null as unknown as { role: string; content: unknown },
        { role: "user", content: "hi" },
      ],
    });
    assert.deepEqual(messages, [
      { role: "system", content: "be brief" },
      { role: "user", content: "hi" },
    ]);
  });
});

describe("serializeTools", () => {
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
  });

  it("returns undefined for empty / missing tools", () => {
    assert.equal(serializeTools(undefined), undefined);
    assert.equal(serializeTools([]), undefined);
  });
});
