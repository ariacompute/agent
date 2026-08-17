import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { registerAriaEngine } from "../src/engine.ts";
import { registerAriaMemo } from "../src/memo.ts";
import { AriaError, ErrorCode, loadConfig, type RunCommand } from "../../packages/aria-bridge/src/index.ts";

describe("pi aria-engine", () => {
  it("registerProvider once with id aria", async () => {
    const providers: unknown[] = [];
    await registerAriaEngine(
      {
        registerProvider(p) {
          providers.push(p);
        },
      },
      {
        createProvider: (opts) => opts,
        openAICompletionsApi: () => ({ id: "openai-completions" }),
        listModelsFn: async () => [{ id: "tiny", ownedBy: "aria" }],
      },
    );
    assert.equal(providers.length, 1);
    const p = providers[0] as { id: string; models: Array<{ id: string }>; api: { id: string } };
    assert.equal(p.id, "aria");
    assert.equal(p.models[0]?.id, "tiny");
    assert.equal(p.api.id, "openai-completions");
  });

  it("empty catalog when engine is down, error recorded", async () => {
    const providers: unknown[] = [];
    await registerAriaEngine(
      {
        registerProvider(p) {
          providers.push(p);
        },
      },
      {
        createProvider: (opts) => opts,
        openAICompletionsApi: () => ({ id: "openai-completions" }),
        listModelsFn: async () => {
          throw new AriaError("down", ErrorCode.ENGINE_UNREACHABLE);
        },
      },
    );
    const p = providers[0] as { models: unknown[]; catalogError: string };
    assert.deepEqual(p.models, []);
    assert.match(p.catalogError, /down/);
  });
});

describe("pi aria-memo", () => {
  it("registers five tools and memo-search command", () => {
    const tools: string[] = [];
    const commands: string[] = [];
    const runCommand: RunCommand = async () => ({ code: 0, stdout: "ok\n", stderr: "" });
    registerAriaMemo(
      {
        registerTool(def) {
          tools.push((def as { name: string }).name);
        },
        registerCommand(name) {
          commands.push(name);
        },
        on() {},
      },
      { config: loadConfig({ ARIA_MEMO_DB: "/tmp/t.db" }), runCommand },
    );
    assert.deepEqual(tools, [
      "aria_memo_add",
      "aria_memo_search",
      "aria_memo_get",
      "aria_memo_list",
      "aria_memo_forget",
    ]);
    assert.deepEqual(commands, ["memo-search"]);
  });

  it("autoInject hooks before_agent_start", async () => {
    let handler: ((event: { prompt?: string }) => unknown) | undefined;
    const runCommand: RunCommand = async () => ({
      code: 0,
      stdout: "0.500\tremember this\n",
      stderr: "",
    });
    registerAriaMemo(
      {
        registerTool() {},
        registerCommand() {},
        on(event, fn) {
          if (event === "before_agent_start") {
            handler = fn as typeof handler;
          }
        },
      },
      { autoInject: true, config: loadConfig({ ARIA_MEMO_DB: "/tmp/t.db" }), runCommand },
    );
    const result = (await handler?.({ prompt: "what did I say" })) as {
      message: { content: string };
    };
    assert.match(result.message.content, /remember this/);
  });
});
