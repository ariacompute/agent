import { describe, it, expect } from "bun:test";
import type { RunCommand } from "../../shared/src/index.ts";
import { loadConfig } from "../../shared/src/index.ts";
import { apply, lastUserText } from "../src/index.ts";

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
    expect(names).toEqual([
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
      expect(args.includes("search")).toBe(true);
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
    expect(listener).toBeDefined();
    let nextCalled = false;
    const out = await listener?.(
      { messages: [{ role: "user", content: "rust?" }] },
      (v) => {
        nextCalled = true;
        return v;
      },
    );
    expect(nextCalled).toBe(true);
    const messages = (out as { messages: Array<{ content: string }> }).messages;
    expect(messages[0].content).toMatch(/aria-memo/);
    expect(messages[0].content).toMatch(/user likes rust/);
  });

  it("extracts last user text", () => {
    expect(
      lastUserText({ messages: [{ role: "user", content: [{ type: "text", text: "hello" }] }] }),
    ).toBe("hello");
  });
});
