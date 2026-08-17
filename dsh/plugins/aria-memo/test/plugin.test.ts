import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { apply, lastUserText } from "../src/index.ts";
import type { RunCommand } from "../../../../packages/aria-bridge/src/index.ts";
import { loadConfig } from "../../../../packages/aria-bridge/src/index.ts";

describe("aria-memo plugin", () => {
  it("registers five tools", () => {
    const names: string[] = [];
    apply(
      {
        llm: { registerAdapter() {} },
        tools: {
          register(def) {
            names.push((def as { name: string }).name);
          },
        },
        on() {},
      },
      {},
    );
    assert.deepEqual(names, [
      "aria_memo_add",
      "aria_memo_search",
      "aria_memo_get",
      "aria_memo_list",
      "aria_memo_forget",
    ]);
  });

  it("autoInject rewrites pre-step messages and calls next", async () => {
    let listener: ((value: unknown, next: (v?: unknown) => unknown) => unknown) | undefined;
    const runCommand: RunCommand = async (_cmd, args) => {
      assert.ok(args.includes("search"));
      return { code: 0, stdout: "0.800\tuser likes rust\n", stderr: "" };
    };
    apply(
      {
        llm: { registerAdapter() {} },
        tools: { register() {} },
        on(event, fn) {
          if (event === "agent/pre-step") {
            listener = fn as typeof listener;
          }
        },
      },
      { autoInject: true, topK: 3 },
      { config: loadConfig({ ARIA_MEMO_DB: "/tmp/t.db" }), runCommand },
    );
    assert.ok(listener);
    let nextCalled = false;
    const out = await listener?.(
      { messages: [{ role: "user", content: "rust?" }] },
      (v) => {
        nextCalled = true;
        return v;
      },
    );
    assert.equal(nextCalled, true);
    const messages = (out as { messages: Array<{ content: string }> }).messages;
    assert.match(messages[0].content, /aria-memo/);
    assert.match(messages[0].content, /user likes rust/);
  });

  it("extracts last user text", () => {
    assert.equal(
      lastUserText({ messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }] }),
      "hello",
    );
  });
});
