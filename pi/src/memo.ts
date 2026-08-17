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
} from "../../packages/aria-bridge/src/index.ts";

export interface PiMemoApi {
  registerTool(def: unknown): void;
  registerCommand(name: string, options: unknown): void;
  on(event: string, handler: (...args: unknown[]) => unknown): void;
}

export interface MemoExtensionOptions {
  autoInject?: boolean;
  topK?: number;
  config?: AriaBridgeConfig;
  runCommand?: RunCommand;
}

function params(properties: Record<string, { type: string; description?: string }>, required: string[]) {
  return { type: "object", properties, required };
}

export function registerAriaMemo(pi: PiMemoApi, options: MemoExtensionOptions = {}): void {
  const bridge = options.config ?? loadConfig();
  const run = { config: bridge, runCommand: options.runCommand };
  const topKDefault = options.topK ?? 5;

  pi.registerTool({
    name: "aria_memo_add",
    label: "Aria memo add",
    description: "Store a local Aria memo.",
    parameters: params(
      {
        content: { type: "string", description: "Memo text" },
        type: { type: "string", description: "working | short_term | long_term:*" },
        importance: { type: "number", description: "0..1" },
      },
      ["content"],
    ),
    async execute(_id: string, args: { content: string; type?: string; importance?: number }, signal: AbortSignal) {
      const id = await memoAdd(args, { ...run, signal });
      return { content: [{ type: "text", text: id }] };
    },
  });

  pi.registerTool({
    name: "aria_memo_search",
    label: "Aria memo search",
    description: "Search local Aria memos. CLI hits are score+content without ids.",
    parameters: params(
      {
        text: { type: "string", description: "Query" },
        top_k: { type: "number", description: "Max hits" },
      },
      ["text"],
    ),
    async execute(_id: string, args: { text: string; top_k?: number }, signal: AbortSignal) {
      const hits = await memoSearch({ text: args.text, topK: args.top_k }, { ...run, signal });
      return { content: [{ type: "text", text: formatSearchHits(hits) }] };
    },
  });

  pi.registerTool({
    name: "aria_memo_get",
    label: "Aria memo get",
    description: "Fetch one Aria memo by id.",
    parameters: params({ id: { type: "string", description: "Memo id" } }, ["id"]),
    async execute(_id: string, args: { id: string }, signal: AbortSignal) {
      const memo = await memoGet(args.id, { ...run, signal });
      return { content: [{ type: "text", text: memo ? JSON.stringify(memo) : "not found" }] };
    },
  });

  pi.registerTool({
    name: "aria_memo_list",
    label: "Aria memo list",
    description: "List Aria memos.",
    parameters: params({ type: { type: "string", description: "Optional type filter" } }, []),
    async execute(_id: string, args: { type?: string }, signal: AbortSignal) {
      const memos = await memoList({ type: args.type }, { ...run, signal });
      return { content: [{ type: "text", text: JSON.stringify(memos) }] };
    },
  });

  pi.registerTool({
    name: "aria_memo_forget",
    label: "Aria memo forget",
    description: "Delete one Aria memo by id.",
    parameters: params({ id: { type: "string", description: "Memo id" } }, ["id"]),
    async execute(_id: string, args: { id: string }, signal: AbortSignal) {
      const forgotten = await memoForget(args.id, { ...run, signal });
      return { content: [{ type: "text", text: forgotten ? "forgotten" : "not found" }] };
    },
  });

  pi.registerCommand("memo-search", {
    description: "Search Aria memos",
    async handler(args: string) {
      const text = args.trim();
      if (!text) {
        throw new Error("usage: /memo-search <query>");
      }
      const hits = await memoSearch({ text, topK: topKDefault }, run);
      return formatSearchHits(hits);
    },
  });

  if (options.autoInject) {
    pi.on("before_agent_start", async (...args: unknown[]) => {
      const event = args[0] as { prompt?: string };
      const prompt = event.prompt?.trim();
      if (!prompt) {
        return;
      }
      const hits = await memoSearch({ text: prompt, topK: topKDefault }, run);
      if (hits.length === 0) {
        return;
      }
      return {
        message: {
          customType: "aria-memo",
          content: `[aria-memo]\n${formatSearchHits(hits)}`,
          display: false,
        },
      };
    });
  }
}
