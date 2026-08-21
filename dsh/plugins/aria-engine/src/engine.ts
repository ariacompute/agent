import type { GenerateOptions } from "@deepseek-ai/dsh-llm";

export const ENGINE_BUNDLE_HINT =
  "set ARIA_ENGINE_BUNDLE to the engine bundle path and ARIA_FFI_LIB to the native lib (libaria_ffi.so)";

export interface ChatMessage {
  role: string;
  content: string;
}

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
}

export type EngineStreamEvent =
  | { type: "text"; text: string }
  | {
      /** A complete tool call (arguments already assembled). `index` is the OpenAI wire index. */
      type: "tool-call";
      index: number;
      id: string;
      name: string;
      arguments: string;
    }
  | { type: "usage"; usage: TokenUsage }
  | { type: "finish"; reason: "stop" | "length" | "tool_calls" };

export interface ModelInfo {
  id: string;
  ownedBy?: string;
}

function parseUsage(raw: unknown): TokenUsage | undefined {
  if (!raw || typeof raw !== "object") {
    return undefined;
  }
  const u = raw as { prompt_tokens?: number; completion_tokens?: number };
  if (typeof u.prompt_tokens !== "number" && typeof u.completion_tokens !== "number") {
    return undefined;
  }
  return {
    inputTokens: u.prompt_tokens ?? 0,
    outputTokens: u.completion_tokens ?? 0,
  };
}

function finishReason(raw: unknown): Extract<EngineStreamEvent, { type: "finish" }>["reason"] {
  if (raw === "length") {
    return "length";
  }
  if (raw === "tool_calls") {
    return "tool_calls";
  }
  return "stop";
}

/**
 * Normalize the JSON object returned by @ariacompute/engine-ts Engine.complete
 * into engine stream events (text + tool-call + usage + finish).
 *
 * Expected shape:
 *   { choices: [{ message: { content, tool_calls: [{ id, function: { name, arguments } }] } }],
 *     usage: { prompt_tokens, completion_tokens }, finish_reason }
 */
export function parseEngineResult(result: unknown): EngineStreamEvent[] {
  const out: EngineStreamEvent[] = [];
  const obj = result as {
    usage?: unknown;
    choices?: Array<{
      message?: {
        content?: string;
        tool_calls?: Array<{
          id?: string;
          function?: { name?: string; arguments?: string };
        }>;
      };
      finish_reason?: string | null;
    }>;
  };
  const choice = obj.choices?.[0];
  const message = choice?.message;
  if (message) {
    if (typeof message.content === "string" && message.content.length > 0) {
      out.push({ type: "text", text: message.content });
    }
    const toolCalls = message.tool_calls;
    if (Array.isArray(toolCalls)) {
      for (let i = 0; i < toolCalls.length; i++) {
        const tc = toolCalls[i];
        const fn = tc.function;
        out.push({
          type: "tool-call",
          index: i,
          id: tc.id ?? `call_${i}`,
          name: fn?.name ?? "",
          arguments: typeof fn?.arguments === "string" ? fn.arguments : "",
        });
      }
    }
  }
  const usage = parseUsage(obj.usage);
  if (usage) {
    out.push({ type: "usage", usage });
  }
  out.push({
    type: "finish",
    reason: finishReason(choice?.finish_reason ?? "stop"),
  });
  return out;
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

function isToolCallBlock(
  block: unknown,
): block is { id?: unknown; callId?: unknown; name?: unknown; arguments?: unknown } {
  return !!block && typeof block === "object" && (block as { type?: string }).type === "tool-call";
}

function isToolResultBlock(
  block: unknown,
): block is { toolCallId?: unknown; content?: unknown; isError?: boolean } {
  return (
    !!block && typeof block === "object" && (block as { type?: string }).type === "tool-result"
  );
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
