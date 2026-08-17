import type { Context } from "@deepseek-ai/cordis";
import {
  formatSearchHits,
  loadConfig,
  memoAdd,
  memoForget,
  memoGet,
  memoList,
  memoSearch,
  type AriaBridgeConfig,
  type RunCommand,
} from "../../../../packages/aria-bridge/src/index.ts";

export const name = "aria-memo";
export const inject = ["tools"];

export interface Config {
  autoInject?: boolean;
  topK?: number;
  memoBin?: string;
  memoDb?: string;
}

export interface MemoPluginDeps {
  config?: AriaBridgeConfig;
  runCommand?: RunCommand;
}

function toolConfig(config: Config): AriaBridgeConfig {
  const base = loadConfig();
  return {
    engineUrl: base.engineUrl,
    memoBin: config.memoBin ?? base.memoBin,
    memoDb: config.memoDb ?? base.memoDb,
  };
}

function contentToText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((block) => {
      if (block && typeof block === "object" && "text" in block && typeof block.text === "string") {
        return block.text;
      }
      return "";
    })
    .join("");
}

export function lastUserText(value: unknown): string | undefined {
  if (!value || typeof value !== "object") {
    return undefined;
  }
  const v = value as Record<string, unknown>;
  if (typeof v.prompt === "string" && v.prompt.trim()) {
    return v.prompt;
  }
  const messages = v.messages;
  if (!Array.isArray(messages)) {
    return undefined;
  }
  for (let i = messages.length - 1; i >= 0; i--) {
    const msg = messages[i] as { role?: string; content?: unknown };
    if (msg?.role === "user") {
      const text = contentToText(msg.content);
      if (text.trim()) {
        return text;
      }
    }
  }
  return undefined;
}

function memoTools(bridge: AriaBridgeConfig, deps: MemoPluginDeps) {
  const options = { config: deps.config ?? bridge, runCommand: deps.runCommand };
  return [
    {
      name: "aria_memo_add",
      description: "Store a local Aria memo (working / short_term / long_term:*).",
      parameters: {
        content: { type: "string", required: true, description: "Memo text" },
        type: { type: "string", description: "working | short_term | long_term:episodic|semantic|entity|graph" },
        importance: { type: "number", description: "0..1" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { id: string }) => [{ type: "text", text: value.id }],
      },
      async execute(args: { content: string; type?: string; importance?: number }, exec: { signal?: AbortSignal }) {
        const id = await memoAdd(args, { ...options, signal: exec.signal });
        return { id };
      },
    },
    {
      name: "aria_memo_search",
      description: "Hybrid-search local Aria memos. Hits are score+content; ids are not returned by the CLI.",
      parameters: {
        text: { type: "string", required: true, description: "Query text" },
        top_k: { type: "number", description: "Max hits (default 5)" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { hits: unknown }) => [
          { type: "text", text: JSON.stringify(value.hits) },
        ],
      },
      async execute(args: { text: string; top_k?: number }, exec: { signal?: AbortSignal }) {
        const hits = await memoSearch({ text: args.text, topK: args.top_k }, { ...options, signal: exec.signal });
        return { hits };
      },
    },
    {
      name: "aria_memo_get",
      description: "Fetch one Aria memo by id.",
      parameters: {
        id: { type: "string", required: true, description: "Memo id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { found: boolean }) => [
          { type: "text", text: value.found ? "found" : "not found" },
        ],
      },
      async execute(args: { id: string }, exec: { signal?: AbortSignal }) {
        const memo = await memoGet(args.id, { ...options, signal: exec.signal });
        return { found: memo !== null, memo };
      },
    },
    {
      name: "aria_memo_list",
      description: "List Aria memos, optionally filtered by type.",
      parameters: {
        type: { type: "string", description: "Optional memo type filter" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { memos: unknown }) => [
          { type: "text", text: JSON.stringify(value.memos) },
        ],
      },
      async execute(args: { type?: string }, exec: { signal?: AbortSignal }) {
        const memos = await memoList({ type: args.type }, { ...options, signal: exec.signal });
        return { memos };
      },
    },
    {
      name: "aria_memo_forget",
      description: "Delete one Aria memo by id.",
      parameters: {
        id: { type: "string", required: true, description: "Memo id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { forgotten: boolean }) => [
          { type: "text", text: value.forgotten ? "forgotten" : "not found" },
        ],
      },
      async execute(args: { id: string }, exec: { signal?: AbortSignal }) {
        const forgotten = await memoForget(args.id, { ...options, signal: exec.signal });
        return { forgotten };
      },
    },
  ];
}

export function apply(ctx: Context, config: Config = {}, deps: MemoPluginDeps = {}): void {
  const bridge = toolConfig(config);
  for (const tool of memoTools(bridge, deps)) {
    ctx.tools.register(tool);
  }

  const autoInject = config.autoInject === true;
  const topK = config.topK ?? 5;
  ctx.on("agent/pre-step", async (...args: unknown[]) => {
    const value = args[0];
    const next = args[1] as (v?: unknown) => unknown;
    if (!autoInject) {
      return next(value);
    }
    const text = lastUserText(value);
    if (!text) {
      return next(value);
    }
    const hits = await memoSearch({ text, topK }, { config: deps.config ?? bridge, runCommand: deps.runCommand });
    if (hits.length === 0) {
      return next(value);
    }
    const block = `[aria-memo]\n${formatSearchHits(hits)}`;
    if (value && typeof value === "object" && Array.isArray((value as { messages?: unknown }).messages)) {
      const current = value as { messages: unknown[] };
      return next({
        ...current,
        messages: [{ role: "user", content: block }, ...current.messages],
      });
    }
    return next(value);
  });
}
