import { describe, expect, test } from "bun:test";
import {
  AriaError,
  DEFAULT_BETA_HEADER,
  getJson,
  postJson,
  readSse,
  resolveClient,
} from "../src/index.js";
import type { ResolvedClient } from "../src/transport.js";

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as any;
}

function fakeFetch(handler: (url: string, init: any) => any): typeof globalThis.fetch {
  return (async (url: string, init: any) => handler(url, init)) as unknown as typeof globalThis.fetch;
}

describe("resolveClient", () => {
  test("throws a config error when no fetch is available", () => {
    const saved = (globalThis as any).fetch;
    delete (globalThis as any).fetch;
    const prevEnv = process.env.ARIA_AGENT_BASE_URL;
    delete process.env.ARIA_AGENT_BASE_URL;
    try {
      expect(() => resolveClient({})).toThrow(AriaError);
      expect(() => resolveClient({})).toThrow(/fetch/);
    } finally {
      (globalThis as any).fetch = saved;
      if (prevEnv) process.env.ARIA_AGENT_BASE_URL = prevEnv;
    }
  });

  test("prefers explicit options over env defaults", () => {
    const prevUrl = process.env.ARIA_AGENT_BASE_URL;
    const prevKey = process.env.ARIA_AGENT_API_KEY;
    process.env.ARIA_AGENT_BASE_URL = "http://env.test";
    process.env.ARIA_AGENT_API_KEY = "env-key";
    try {
      const c: ResolvedClient = resolveClient({
        baseUrl: "http://opt.test",
        apiKey: "opt-key",
        fetch: fakeFetch(() => jsonResponse({})),
      });
      expect(c.baseUrl).toBe("http://opt.test");
      expect(c.apiKey).toBe("opt-key");
      expect(c.betaHeader).toBe(DEFAULT_BETA_HEADER);

      const d: ResolvedClient = resolveClient({ fetch: fakeFetch(() => jsonResponse({})) });
      expect(d.baseUrl).toBe("http://env.test");
      expect(d.apiKey).toBe("env-key");
    } finally {
      if (prevUrl) process.env.ARIA_AGENT_BASE_URL = prevUrl;
      else delete process.env.ARIA_AGENT_BASE_URL;
      if (prevKey) process.env.ARIA_AGENT_API_KEY = prevKey;
      else delete process.env.ARIA_AGENT_API_KEY;
    }
  });

  test("strips trailing slashes from the base URL", () => {
    const c: ResolvedClient = resolveClient({
      baseUrl: "http://aria.test///",
      fetch: fakeFetch(() => jsonResponse({})),
    });
    expect(c.baseUrl).toBe("http://aria.test");
  });
});

describe("typed errors", () => {
  test("401 maps to auth, other 4xx/5xx to api", async () => {
    const authClient = resolveClient({
      fetch: fakeFetch(() => jsonResponse({ error: "no" }, 401)),
    });
    await expect(getJson(authClient, "/x")).rejects.toMatchObject({
      kind: "auth",
      status: 401,
    });

    const apiClient = resolveClient({
      fetch: fakeFetch(() => jsonResponse({ error: "bad" }, 404)),
    });
    const err = await getJson(apiClient, "/x").catch((e) => e);
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).kind).toBe("api");
    expect((err as AriaError).status).toBe(404);
  });

  test("network failure maps to network", async () => {
    const client = resolveClient({
      fetch: (async () => {
        throw new Error("conn reset");
      }) as unknown as typeof globalThis.fetch,
    });
    const err = await postJson(client, "/x", {}).catch((e) => e);
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).kind).toBe("network");
  });
});

describe("readSse", () => {
  test("ignores malformed JSON frames and comment lines", async () => {
    const raw =
      ": keep-alive\n\n" +
      'data: not-json\n\n' +
      'data: {"type":"agent.turn.output_text.delta","delta":"ok"}\n\n';
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode(raw));
        c.close();
      },
    });
    const out: string[] = [];
    for await (const f of readSse(stream as unknown as AsyncIterable<Uint8Array>)) {
      out.push(String(f.type));
    }
    expect(out).toEqual(["agent.turn.output_text.delta"]);
  });

  test("passes through a trailing partial frame without hanging", async () => {
    const raw = 'data: {"type":"agent.turn.created"}\n'; // no blank line terminator
    const stream = new ReadableStream<Uint8Array>({
      start(c) {
        c.enqueue(new TextEncoder().encode(raw));
        c.close();
      },
    });
    const out: unknown[] = [];
    for await (const f of readSse(stream as unknown as AsyncIterable<Uint8Array>)) {
      out.push(f);
    }
    expect(out).toEqual([]);
  });
});
