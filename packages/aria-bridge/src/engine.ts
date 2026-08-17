import { AriaError, ErrorCode } from "./error.ts";

export const ENGINE_SERVE_HINT =
  "aria-engine serve <bundle> --bind 127.0.0.1:8080";

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
  | { type: "usage"; usage: TokenUsage }
  | { type: "finish"; reason: "stop" | "length" | "tool_calls" };

export interface ChatStreamRequest {
  engineUrl: string;
  messages: ChatMessage[];
  model?: string;
  temperature?: number;
  maxTokens?: number;
  signal?: AbortSignal;
  fetchImpl?: typeof fetch;
  headers?: Record<string, string>;
}

export interface ModelInfo {
  id: string;
  ownedBy?: string;
}

function unreachableMessage(engineUrl: string, cause: string): string {
  return `aria-engine is not reachable at ${engineUrl}. Start it with: ${ENGINE_SERVE_HINT} (${cause})`;
}

async function engineFetch(
  url: string,
  init: RequestInit,
  fetchImpl: typeof fetch,
  engineUrl: string,
): Promise<Response> {
  try {
    return await fetchImpl(url, init);
  } catch (err) {
    const cause = err instanceof Error ? err.message : String(err);
    throw new AriaError(unreachableMessage(engineUrl, cause), ErrorCode.ENGINE_UNREACHABLE);
  }
}

export async function listModels(
  engineUrl: string,
  options: { signal?: AbortSignal; fetchImpl?: typeof fetch; headers?: Record<string, string> } = {},
): Promise<ModelInfo[]> {
  const fetchImpl = options.fetchImpl ?? fetch;
  const res = await engineFetch(
    `${engineUrl}/models`,
    { method: "GET", signal: options.signal, headers: options.headers },
    fetchImpl,
    engineUrl,
  );
  if (!res.ok) {
    const body = await res.text();
    throw new AriaError(
      `GET ${engineUrl}/models failed: HTTP ${res.status} ${body}`.trim(),
      ErrorCode.ENGINE_HTTP,
    );
  }
  const json: unknown = await res.json();
  const data = (json as { data?: Array<{ id?: string; owned_by?: string }> }).data;
  if (!Array.isArray(data)) {
    return [];
  }
  return data
    .filter((row) => typeof row.id === "string" && row.id.length > 0)
    .map((row) => ({ id: row.id as string, ownedBy: row.owned_by }));
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

/** Parse OpenAI SSE body into engine stream events. */
export function* parseSseBody(body: string): Generator<EngineStreamEvent> {
  const blocks = body.replace(/\r\n/g, "\n").split("\n\n");
  let emittedFinish = false;
  for (const block of blocks) {
    const dataLines = block
      .split("\n")
      .filter((line) => line.startsWith("data:"))
      .map((line) => line.slice("data:".length).trimStart());
    if (dataLines.length === 0) {
      continue;
    }
    const data = dataLines.join("\n").trim();
    if (data === "[DONE]") {
      if (!emittedFinish) {
        emittedFinish = true;
        yield { type: "finish", reason: "stop" };
      }
      return;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(data);
    } catch {
      throw new AriaError(`invalid SSE JSON: ${data}`, ErrorCode.ENGINE_HTTP);
    }
    const obj = parsed as {
      usage?: unknown;
      choices?: Array<{ delta?: { content?: string }; finish_reason?: string | null }>;
    };
    const usage = parseUsage(obj.usage);
    if (usage) {
      yield { type: "usage", usage };
    }
    const choice = obj.choices?.[0];
    const text = choice?.delta?.content;
    if (typeof text === "string" && text.length > 0) {
      yield { type: "text", text };
    }
    if (choice?.finish_reason) {
      emittedFinish = true;
      yield { type: "finish", reason: finishReason(choice.finish_reason) };
    }
  }
  if (!emittedFinish) {
    yield { type: "finish", reason: "stop" };
  }
}

export async function* chatStream(
  req: ChatStreamRequest,
): AsyncGenerator<EngineStreamEvent> {
  const fetchImpl = req.fetchImpl ?? fetch;
  const res = await engineFetch(
    `${req.engineUrl}/chat/completions`,
    {
      method: "POST",
      signal: req.signal,
      headers: {
        "content-type": "application/json",
        ...(req.headers ?? {}),
      },
      body: JSON.stringify({
        model: req.model,
        messages: req.messages,
        stream: true,
        temperature: req.temperature,
        max_tokens: req.maxTokens,
      }),
    },
    fetchImpl,
    req.engineUrl,
  );
  if (!res.ok) {
    const body = await res.text();
    throw new AriaError(
      `POST ${req.engineUrl}/chat/completions failed: HTTP ${res.status} ${body}`.trim(),
      ErrorCode.ENGINE_HTTP,
    );
  }
  const text = await res.text();
  yield* parseSseBody(text);
}
