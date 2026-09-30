import { describe, expect, test } from "bun:test";
import { Agent, Session } from "../src/index.js";
import type { ClientOptions } from "../src/types.js";

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  } as any;
}

function fakeFetch(handler: (url: string, init: any) => any) {
  const calls: { url: string; init: any }[] = [];
  const impl = (async (url: string, init: any) => {
    calls.push({ url, init });
    return handler(url, init);
  }) as unknown as typeof globalThis.fetch;
  return { impl, calls };
}

const client: ClientOptions = { baseUrl: "http://aria.test", apiKey: "aria-test" };

describe("Session getItems / getTurns (read-only)", () => {
  test("getItems hits the items endpoint", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse([{ id: "i1" }]));
    const session = new Session("sess_1", { ...client, fetch: impl });
    const items = await session.getItems();
    expect(items).toEqual([{ id: "i1" }]);
    expect(calls[0].url).toBe("http://aria.test/v1/agents/sessions/sess_1/items");
    expect(calls[0].init.method).toBe("GET");
  });

  test("getTurns hits the turns endpoint", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse([{ id: "t1", status: "done" }]));
    const session = new Session("sess_1", { ...client, fetch: impl });
    const turns = await session.getTurns();
    expect(turns).toEqual([{ id: "t1", status: "done" }]);
    expect(calls[0].url).toBe("http://aria.test/v1/agents/sessions/sess_1/turns");
  });

  test("read-only endpoints propagate typed auth errors", async () => {
    const { impl } = fakeFetch(() => jsonResponse({ error: "forbidden" }, 403));
    const session = new Session("sess_1", { ...client, fetch: impl });
    await expect(session.getItems()).rejects.toMatchObject({ kind: "auth", status: 403 });
  });
});

describe("Session backend guards", () => {
  test("unknown backend override is rejected on memorize and recall", async () => {
    const session = new Session("sess_1", client);
    // @ts-expect-error runtime guard check
    await expect(session.memorize("k", "v", { backend: "nonsense" })).rejects.toThrow(
      /unknown memory backend/,
    );
    // @ts-expect-error runtime guard check
    await expect(session.recall("k", { backend: "nonsense" })).rejects.toThrow(
      /unknown memory backend/,
    );
  });

  test("record appends the user input and assistant reply", () => {
    const session = new Session("sess_1", client);
    session.record("hi", "hello");
    expect(session.history).toEqual([
      "hi",
      { role: "assistant", content: "hello" },
    ]);
  });
});

describe("Session.create", () => {
  test("posts agent id when present, else the agent name", async () => {
    const byName = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "s1" });
      return jsonResponse({});
    });
    const a1 = new Agent({ name: "tutor", client: { ...client, fetch: byName.impl } });
    const s1 = await Session.create(a1);
    expect(JSON.parse(byName.calls[0].init.body)).toEqual({ agent: "tutor" });

    const byId = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "s2" });
      return jsonResponse({});
    });
    const a2 = new Agent({
      name: "tutor",
      id: "agent_42",
      client: { ...client, fetch: byId.impl },
    });
    await Session.create(a2);
    expect(JSON.parse(byId.calls[0].init.body)).toEqual({ agent_id: "agent_42" });
  });
});
