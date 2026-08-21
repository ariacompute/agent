export interface GenerateOptions {
  provider: string;
  model?: string;
  messages: unknown[];
  system?: string;
  tools?: unknown[];
  temperature?: number;
  maxTokens?: number;
  stop?: string[];
  signal?: AbortSignal;
  sessionId?: string;
}

export interface TokenUsage {
  inputTokens: number;
  outputTokens: number;
  cacheReadTokens?: number;
  cacheWriteTokens?: number;
  reasoningTokens?: number;
}

export type StreamChunk =
  | { type: "block-start"; index: number; blockType: string }
  | { type: "text-delta"; index: number; text: string }
  | { type: "tool-call-delta"; index: number; id: string; name: string; argumentsDelta: string }
  | {
      type: "block-end";
      index: number;
      block:
        | { type: "text"; text: string }
        | { type: "tool-call"; id: string; name: string; arguments: string };
    }
  | { type: "usage"; usage: TokenUsage }
  | {
      type: "finish";
      reason: { kind: "stop" | "tool-calls" | "max-tokens" | "aborted" | "error"; failure?: { message: string; code: string } };
    };

export interface LlmModelInfo {
  provider: string;
  id: string;
  name: string;
}

export class LlmError extends Error {
  readonly code: string;
  constructor(message: string, code: string) {
    super(message);
    this.name = "LlmError";
    this.code = code;
  }
}

export function attributionHeaders(): Record<string, string> {
  return {
    "user-agent": "deepseek-harness/aria-adapter (+https://github.com/deepseek-ai/deepseek-harness)",
  };
}
