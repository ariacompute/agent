import type { GenerateOptions, StreamChunk } from "@deepseek-ai/dsh-llm";
import {
  chatStream,
  listModels,
  type ChatMessage,
  type EngineStreamEvent,
} from "./engine.ts";

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

export function openaiMessagesFrom(options: GenerateOptions): ChatMessage[] {
  const out: ChatMessage[] = [];
  if (options.system && options.system.length > 0) {
    out.push({ role: "system", content: options.system });
  }
  for (const raw of options.messages) {
    if (!raw || typeof raw !== "object") {
      continue;
    }
    const msg = raw as { role?: string; content?: unknown };
    const role = msg.role ?? "user";
    const text = contentToText(msg.content);
    out.push({ role, content: text });
  }
  return out;
}

export async function* toDshChunks(
  events: AsyncIterable<EngineStreamEvent>,
): AsyncGenerator<StreamChunk> {
  let started = false;
  let assembled = "";
  let pendingUsage: { inputTokens: number; outputTokens: number } | undefined;
  let finish: StreamChunk | undefined;

  for await (const ev of events) {
    if (ev.type === "text") {
      if (!started) {
        yield { type: "block-start", index: 0, blockType: "text" };
        started = true;
      }
      assembled += ev.text;
      yield { type: "text-delta", index: 0, text: ev.text };
    } else if (ev.type === "usage") {
      pendingUsage = ev.usage;
    } else if (ev.type === "finish") {
      const kind =
        ev.reason === "length" ? "max-tokens" : ev.reason === "tool_calls" ? "tool-calls" : "stop";
      finish = { type: "finish", reason: { kind } };
    }
  }

  if (started) {
    yield { type: "block-end", index: 0, block: { type: "text", text: assembled } };
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
      signal: options.signal,
      fetchImpl: this.config.fetchImpl,
      headers: { "user-agent": ARIA_USER_AGENT },
    });
    yield* toDshChunks(events);
  }
}
