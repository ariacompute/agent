import type { GenerateOptions, StreamChunk } from "@deepseek-ai/dsh-llm";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import {
  defaultEngineFactory,
  generate,
  type EngineFactory,
} from "./engine-binding.ts";
import { openaiMessagesFrom, serializeTools, type EngineStreamEvent } from "./engine.ts";

export { openaiMessagesFrom, serializeTools } from "./engine.ts";

export interface AriaAdapterConfig {
  /** Local engine bundle path for in-process FFI. */
  bundlePath: string;
  /** Path to the native FFI shared library (ARIA_FFI_LIB). */
  ffiLib?: string;
  defaultModel?: string;
  /** Override the engine factory (for testing). Defaults to the real SDK. */
  engineFactory?: EngineFactory;
}

export async function* toDshChunks(
  events: AsyncIterable<EngineStreamEvent> | EngineStreamEvent[],
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
  private readonly factory: EngineFactory;

  constructor(private readonly config: AriaAdapterConfig) {
    this.factory = config.engineFactory ?? defaultEngineFactory;
  }

  providerInfo(provider: string): { id: string; name: string } {
    return { id: provider, name: "Aria Engine" };
  }

  async listModels(
    provider: string,
  ): Promise<Array<{ provider: string; id: string; name: string }>> {
    if (!this.config.defaultModel) {
      throw new AriaError(
        "in-process engine has no model registry; set adapter config.defaultModel",
        ErrorCode.ENGINE,
      );
    }
    return [{ provider, id: this.config.defaultModel, name: this.config.defaultModel }];
  }

  async resolveModel(
    provider: string,
    model: string,
  ): Promise<{ provider: string; id: string; name: string }> {
    const id = model || this.config.defaultModel;
    if (!id) {
      throw new AriaError(
        "in-process engine requires an explicit model (config.defaultModel or request model)",
        ErrorCode.ENGINE,
      );
    }
    return { provider, id, name: id };
  }

  async *stream(options: GenerateOptions): AsyncIterable<StreamChunk> {
    if (options.stop && options.stop.length > 0) {
      const err = new Error("aria-engine does not support stop sequences") as Error & {
        code: string;
      };
      err.code = "UNSUPPORTED";
      throw err;
    }
    const model = options.model || this.config.defaultModel;
    if (!model) {
      throw new AriaError(
        "in-process engine requires a model (config.defaultModel or request model)",
        ErrorCode.ENGINE,
      );
    }
    const events = generate(
      this.factory,
      this.config.bundlePath,
      this.config.ffiLib,
      openaiMessagesFrom(options),
      {
        model,
        temperature: options.temperature,
        max_tokens: options.maxTokens,
      },
      serializeTools(options.tools),
    );
    yield* toDshChunks(events);
  }
}
