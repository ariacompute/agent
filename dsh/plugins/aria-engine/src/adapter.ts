import type { GenerateOptions, StreamChunk } from "@deepseek-ai/dsh-llm";
import { chatStream, listModels, type EngineStreamEvent } from "./engine.ts";

export const ARIA_USER_AGENT =
  "deepseek-harness/aria-adapter (+https://github.com/deepseek-ai/deepseek-harness)";

export interface AriaAdapterConfig {
  engineUrl: string;
  defaultModel?: string;
  fetchImpl?: typeof fetch;
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

function isToolCallBlock(block: unknown): block is { id?: unknown; callId?: unknown; name?: unknown; arguments?: unknown } {
  return !!block && typeof block === "object" && (block as { type?: string }).type === "tool-call";
}

function isToolResultBlock(block: unknown): block is { toolCallId?: unknown; content?: unknown; isError?: boolean } {
  return !!block && typeof block === "object" && (block as { type?: string }).type === "tool-result";
}

/** dsh tools (function tools) -> OpenAI wire `tools` (function type). */
export function serializeTools(tools?: unknown[]): unknown[] | undefined {
  if (!tools || tools.length === 0) {
    return undefined;
  }
  return tools.map((tool) => {
    const t = (tool ?? {}) as { name?: string; description?: string; parameters?: unknown };
    return {
      type: "function",
      function: {
        name: t.name ?? "",
        description: t.description ?? "",
        ...(t.parameters !== undefined ? { parameters: t.parameters } : {}),
      },
    };
  });
}

/** Serialize dsh conversation history to OpenAI wire messages (tool calls + tool results included). */
export function openaiMessagesFrom(options: GenerateOptions): unknown[] {
  const out: unknown[] = [];
  if (options.system && options.system.length > 0) {
    out.push({ role: "system", content: options.system });
  }
  for (const raw of options.messages) {
    if (!raw || typeof raw !== "object") {
      continue;
    }
    const msg = raw as { role?: string; content?: unknown };
    const role = msg.role ?? "user";
    const blocks = Array.isArray(msg.content) ? msg.content : [];
    if (role === "user" && blocks.length === 1 && isToolResultBlock(blocks[0])) {
      const tr = blocks[0];
      out.push({
        role: "tool",
        tool_call_id: typeof tr.toolCallId === "string" ? tr.toolCallId : "",
        content: contentToText(tr.content),
        ...(tr.isError ? { is_error: true } : {}),
      });
      continue;
    }
    if (role === "assistant") {
      const text = contentToText(msg.content);
      const toolCalls = blocks
        .filter(isToolCallBlock)
        .map((tc) => ({
          id: typeof tc.id === "string" ? tc.id : typeof tc.callId === "string" ? tc.callId : "",
          type: "function",
          function: {
            name: typeof tc.name === "string" ? tc.name : "",
            arguments:
              typeof tc.arguments === "string"
                ? tc.arguments
                : JSON.stringify(tc.arguments ?? {}),
          },
        }));
      const assistant: Record<string, unknown> = { role: "assistant" };
      if (text) {
        assistant.content = text;
      } else if (toolCalls.length === 0) {
        assistant.content = "";
      }
      if (toolCalls.length > 0) {
        assistant.tool_calls = toolCalls;
      }
      out.push(assistant);
      continue;
    }
    const text = contentToText(msg.content);
    out.push({ role, content: text });
  }
  return out;
}

export async function* toDshChunks(
  events: AsyncIterable<EngineStreamEvent>,
): AsyncGenerator<StreamChunk> {
  // Text and tool-call blocks share one index space, assigned in arrival order.
  const blockStack: ({ type: "text"; index: number; text: string } | { type: "tool-call"; index: number; wireIndex: number })[] =
    [];
  // OpenAI wire tool-call index -> dsh block index (wire indices are NOT block indices).
  const toolCallBlockIndex = new Map<number, number>();
  const pendingToolCalls: { id: string; name: string; arguments: string }[] = [];
  let pendingUsage: { inputTokens: number; outputTokens: number } | undefined;
  let finish: StreamChunk | undefined;

  const pushText = (text: string): StreamChunk[] => {
    if (!text) {
      return [];
    }
    const top = blockStack[blockStack.length - 1];
    if (top && top.type === "text") {
      top.text += text;
      return [{ type: "text-delta", index: top.index, text }];
    }
    const index = blockStack.length;
    blockStack.push({ type: "text", index, text });
    return [
      { type: "block-start", index, blockType: "text" },
      { type: "text-delta", index, text },
    ];
  };

  for await (const ev of events) {
    if (ev.type === "text") {
      for (const chunk of pushText(ev.text)) {
        yield chunk;
      }
    } else if (ev.type === "tool-call") {
      const wireIndex = ev.index;
      while (pendingToolCalls.length <= wireIndex) {
        pendingToolCalls.push({ id: "", name: "", arguments: "" });
      }
      pendingToolCalls[wireIndex] = { id: ev.id, name: ev.name, arguments: ev.arguments };
      let index = toolCallBlockIndex.get(wireIndex);
      if (index === undefined) {
        index = blockStack.length;
        toolCallBlockIndex.set(wireIndex, index);
        blockStack.push({ type: "tool-call", index, wireIndex });
        yield { type: "block-start", index, blockType: "tool-call" };
      }
      if (ev.arguments) {
        yield {
          type: "tool-call-delta",
          index,
          id: ev.id,
          name: ev.name,
          argumentsDelta: ev.arguments,
        };
      }
    } else if (ev.type === "usage") {
      pendingUsage = ev.usage;
    } else if (ev.type === "finish") {
      const kind =
        ev.reason === "length" ? "max-tokens" : ev.reason === "tool_calls" ? "tool-calls" : "stop";
      finish = { type: "finish", reason: { kind } };
    }
  }

  for (const top of blockStack) {
    if (top.type === "text") {
      yield { type: "block-end", index: top.index, block: { type: "text", text: top.text } };
    } else {
      const tc = pendingToolCalls[top.wireIndex] ?? { id: "", name: "", arguments: "" };
      // Canonical `id` field (not callId): dsh-session drops tool-call blocks without it.
      yield {
        type: "block-end",
        index: top.index,
        block: { type: "tool-call", id: tc.id, name: tc.name, arguments: tc.arguments },
      };
    }
  }
  if (pendingUsage) {
    yield {
      type: "usage",
      usage: { inputTokens: pendingUsage.inputTokens, outputTokens: pendingUsage.outputTokens },
    };
  }
  yield finish ?? { type: "finish", reason: { kind: "stop" } };
}

/** Duck-typed LlmAdapter: no runtime import of @deepseek-ai/dsh-llm (out-of-tree resolve). */
export class AriaAdapter {
  constructor(private readonly config: AriaAdapterConfig) {}

  providerInfo(provider: string): { id: string; name: string } {
    return { id: provider, name: "Aria Engine" };
  }

  async listModels(provider: string): Promise<Array<{ provider: string; id: string; name: string }>> {
    const models = await listModels(this.config.engineUrl, {
      fetchImpl: this.config.fetchImpl,
      headers: { "user-agent": ARIA_USER_AGENT },
    });
    return models.map((m) => ({ provider, id: m.id, name: m.id }));
  }

  async resolveModel(
    provider: string,
    model: string,
    signal?: AbortSignal,
  ): Promise<{ provider: string; id: string; name: string }> {
    const models = await listModels(this.config.engineUrl, {
      signal,
      fetchImpl: this.config.fetchImpl,
      headers: { "user-agent": ARIA_USER_AGENT },
    });
    const hit = models.find((m) => m.id === model);
    return { provider, id: model, name: hit?.id ?? model };
  }

  async *stream(options: GenerateOptions): AsyncIterable<StreamChunk> {
    if (options.stop && options.stop.length > 0) {
      const err = new Error("aria-engine does not support stop sequences") as Error & { code: string };
      err.code = "UNSUPPORTED";
      throw err;
    }
    const events = chatStream({
      engineUrl: this.config.engineUrl,
      messages: openaiMessagesFrom(options),
      model: options.model || this.config.defaultModel,
      temperature: options.temperature,
      maxTokens: options.maxTokens,
      tools: serializeTools(options.tools),
      signal: options.signal,
      fetchImpl: this.config.fetchImpl,
      headers: { "user-agent": ARIA_USER_AGENT },
    });
    yield* toDshChunks(events);
  }
}
