/**
 * HTTP transport for the aria-agent-cloud **beta Agents** API.
 *
 * Only `fetch` is used, so the SDK runs on Node 18+, Bun, Deno and edge runtimes.
 */

import type { ClientOptions } from "./types.js";

export const DEFAULT_BETA_HEADER = "agents=v1";

export interface ResolvedClient {
  baseUrl: string;
  apiKey?: string;
  betaHeader: string;
  fetch: typeof globalThis.fetch;
}

export function resolveClient(options: ClientOptions = {}): ResolvedClient {
  const fetchImpl =
    options.fetch ??
    (globalThis as unknown as { fetch?: typeof globalThis.fetch }).fetch;
  if (!fetchImpl) {
    throw new Error("no fetch implementation available; pass `client.fetch`");
  }
  return {
    baseUrl: (options.baseUrl ?? process.env.ARIA_AGENT_BASE_URL ?? "http://localhost:3000").replace(
      /\/+$/,
      "",
    ),
    apiKey: options.apiKey ?? process.env.ARIA_AGENT_API_KEY ?? undefined,
    betaHeader: options.betaHeader ?? DEFAULT_BETA_HEADER,
    fetch: fetchImpl,
  };
}

function headers(client: ResolvedClient, extra: Record<string, string> = {}): Record<string, string> {
  const h: Record<string, string> = {
    "OpenAI-Beta": client.betaHeader,
    ...extra,
  };
  if (client.apiKey) {
    h["Authorization"] = `Bearer ${client.apiKey}`;
  }
  return h;
}

/** POST JSON and decode a JSON response. Throws on non-2xx. */
export async function postJson<T>(
  client: ResolvedClient,
  path: string,
  body: unknown,
): Promise<T> {
  const res = await client.fetch(`${client.baseUrl}${path}`, {
    method: "POST",
    headers: headers(client, { "Content-Type": "application/json" }),
    body: JSON.stringify(body),
  });
  if (!res.ok) {
    throw new Error(`aria agent request failed (${res.status}): ${await safeText(res)}`);
  }
  return (await res.json()) as T;
}

/** GET JSON. Throws on non-2xx. */
export async function getJson<T>(client: ResolvedClient, path: string): Promise<T> {
  const res = await client.fetch(`${client.baseUrl}${path}`, {
    method: "GET",
    headers: headers(client),
  });
  if (!res.ok) {
    throw new Error(`aria agent request failed (${res.status}): ${await safeText(res)}`);
  }
  return (await res.json()) as T;
}

/** POST JSON and stream the SSE response body, yielding decoded `data:` frames. */
export async function* postJsonStream(
  client: ResolvedClient,
  path: string,
  body: unknown,
): AsyncGenerator<Record<string, any>> {
  const res = await client.fetch(`${client.baseUrl}${path}`, {
    method: "POST",
    headers: headers(client, {
      "Content-Type": "application/json",
      Accept: "text/event-stream",
    }),
    body: JSON.stringify(body),
  });
  if (!res.ok || !res.body) {
    throw new Error(`aria agent stream failed (${res.status}): ${await safeText(res)}`);
  }
  yield* readSse(res.body);
}

/**
 * Decode an SSE body into JSON frames.
 *
 * The aria envelope has **no** `data: [DONE]` sentinel: the terminal frame is
 * `agent.turn.completed` / `agent.turn.failed`, so the stream simply ends when
 * the server closes it.
 */
export async function* readSse(
  body: AsyncIterable<Uint8Array | string>,
): AsyncGenerator<Record<string, any>> {
  const decoder = new TextDecoder();
  let buffer = "";
  for await (const chunk of body as AsyncIterable<any>) {
    buffer += typeof chunk === "string" ? chunk : decoder.decode(chunk, { stream: true });
    let idx: number;
    while ((idx = buffer.indexOf("\n\n")) !== -1) {
      const raw = buffer.slice(0, idx);
      buffer = buffer.slice(idx + 2);
      for (const line of raw.split("\n")) {
        if (!line.startsWith("data:")) continue;
        const data = line.slice(5).trim();
        if (!data || data === "[DONE]") continue;
        try {
          yield JSON.parse(data);
        } catch {
          // Ignore keep-alives / malformed frames.
        }
      }
    }
  }
}

async function safeText(res: { text?: () => Promise<string> }): Promise<string> {
  try {
    return res.text ? await res.text() : "";
  } catch {
    return "";
  }
}
