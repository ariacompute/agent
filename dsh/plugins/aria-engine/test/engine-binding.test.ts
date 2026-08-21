import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { createEngineFactory, generate, type EngineFactory, type EngineLike } from "../src/engine-binding.ts";

function fakeEngine(result: unknown, opts?: { close?: boolean }): { factory: EngineFactory; closed: { value: boolean }; last: { messages?: unknown; options?: unknown; tools?: unknown } } {
  const closed = { value: false };
  const last: { messages?: unknown; options?: unknown; tools?: unknown } = {};
  const engine: EngineLike = {
    complete(messages, options, tools) {
      last.messages = messages;
      last.options = options;
      last.tools = tools;
      return result;
    },
    close() {
      closed.value = opts?.close ?? true;
    },
  };
  const factory: EngineFactory = () => engine;
  return { factory, closed, last };
}

describe("engine-binding generate()", () => {
  it("parses text + tool_calls + usage + finish from SDK result", () => {
    const { factory, closed, last } = fakeEngine({
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
      usage: { prompt_tokens: 3, completion_tokens: 2 },
    });
    const events = generate(
      factory,
      "/bundle",
      "/lib.so",
      [{ role: "user", content: "run ls" }],
      { model: "aria-tiny" },
      [{ type: "function", function: { name: "sandbox_exec" } }],
    );
    assert.equal(closed.value, true);
    assert.deepEqual(last.messages, [{ role: "user", content: "run ls" }]);
    assert.deepEqual(last.options, { model: "aria-tiny" });
    assert.deepEqual(last.tools, [{ type: "function", function: { name: "sandbox_exec" } }]);
    const types = events.map((e) => e.type);
    assert.deepEqual(types, ["text", "tool-call", "usage", "finish"]);
    const tc = events.find((e) => e.type === "tool-call");
    assert.deepEqual(
      tc && tc.type === "tool-call" ? { id: tc.id, name: tc.name, arguments: tc.arguments, index: tc.index } : null,
      { id: "call_1", name: "sandbox_exec", arguments: '{"cmd":"ls"}', index: 0 },
    );
    const finish = events.at(-1);
    assert.equal(finish?.type, "finish");
    assert.equal(finish.type === "finish" ? finish.reason : "", "tool_calls");
    const usage = events.find((e) => e.type === "usage");
    assert.deepEqual(
      usage && usage.type === "usage" ? usage.usage : null,
      { inputTokens: 3, outputTokens: 2 },
    );
  });

  it("maps factory load failure to ENGINE_UNREACHABLE", () => {
    const factory: EngineFactory = () => {
      throw new Error("cannot open bundle");
    };
    assert.throws(
      () => generate(factory, "/missing", "/lib.so", [], {}, undefined),
      (err) => err instanceof AriaError && err.code === ErrorCode.ENGINE_UNREACHABLE,
    );
  });

  it("maps complete() failure to ENGINE error", () => {
    const { factory } = fakeEngine(null);
    factory("destroy").complete = () => {
      throw new Error("native crash");
    };
    // re-create a factory whose engine throws on complete
    const badFactory: EngineFactory = () => ({
      complete: () => {
        throw new Error("native crash");
      },
    });
    assert.throws(
      () => generate(badFactory, "/bundle", "/lib.so", [], {}, undefined),
      (err) => err instanceof AriaError && err.code === ErrorCode.ENGINE,
    );
  });

  it("createEngineFactory sets ARIA_FFI_LIB and constructs the sdk Engine", () => {
    const constructed: { bundle: string }[] = [];
    const sdk = {
      Engine: class {
        constructor(bundle: string) {
          constructed.push({ bundle });
        }
      } as new (bundle: string) => EngineLike,
    };
    const factory = createEngineFactory(sdk);
    factory("/my.bundle", "/my/lib.so");
    assert.deepEqual(constructed, [{ bundle: "/my.bundle" }]);
    assert.equal(process.env.ARIA_FFI_LIB, "/my/lib.so");
  });

  it("close() is optional on the engine", () => {
    const factory: EngineFactory = () => ({ complete: () => ({ choices: [{ message: { content: "x" }, finish_reason: "stop" }] }) });
    const events = generate(factory, "/b", undefined, [], {}, undefined);
    assert.equal(events.at(-1)?.type, "finish");
  });
});
