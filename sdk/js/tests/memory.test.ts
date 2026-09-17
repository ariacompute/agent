import { describe, expect, test } from "bun:test";
import { Agent, Session } from "../src/index.js";
import {
  CompositeMemoryStore,
  LocalMemoryStore,
  decodeLocal,
  encodeLocal,
  type MemoryStore,
} from "../src/index.js";
import type { MemoryBackend } from "../src/types.js";

/** Fake `aria-memo` CLI: records args, replays canned stdout. */
function fakeExec(handler: (args: string[]) => string) {
  const calls: string[][] = [];
  const run = async (args: string[]) => {
    calls.push(args);
    return handler(args);
  };
  return { run, calls };
}

function jsonResponse(body: unknown, status = 200) {
  return {
    ok: status >= 200 && status < 300,
    status,
    json: async () => body,
    text: async () => JSON.stringify(body),
  };
}

function fakeFetch(handler: (url: string, init: any) => any) {
  const calls: { url: string; init: any }[] = [];
  const impl = (async (url: string, init: any) => {
    calls.push({ url, init });
    return handler(url, init);
  }) as unknown as typeof globalThis.fetch;
  return { impl, calls };
}

const client = { baseUrl: "http://aria.test", apiKey: "aria-test" };

describe("local backend (aria-memo CLI)", () => {
  test("put encodes `key: value` and writes a long-term memory", async () => {
    const { run, calls } = fakeExec(() => "");
    const store = new LocalMemoryStore({ memoDb: "/tmp/memo.db", exec: run });
    await store.put("user_name", "Ada");
    expect(store.backend).toBe("local" as MemoryBackend);
    expect(calls[0]).toEqual([
      "--db",
      "/tmp/memo.db",
      "add",
      "--type",
      "long_term:semantic",
      "--content",
      "user_name: Ada",
      "--importance",
      "0.8",
    ]);
  });

  test("get searches by key and strips the prefix", async () => {
    const { run } = fakeExec(() =>
      JSON.stringify([
        { id: "1", score: 0.9, content: "user_name: Ada" },
        { id: "2", score: 0.4, content: "unrelated fact" },
      ]),
    );
    const store = new LocalMemoryStore({ exec: run });
    expect(await store.get("user_name")).toBe("Ada");
  });

  test("get tolerates entries written without the prefix", async () => {
    const { run } = fakeExec(() => JSON.stringify([{ id: "1", content: "Ada" }]));
    const store = new LocalMemoryStore({ exec: run });
    expect(await store.get("user_name")).toBe("Ada");
  });

  test("get returns null when aria memo has nothing", async () => {
    const { run } = fakeExec(() => "");
    const store = new LocalMemoryStore({ exec: run });
    expect(await store.get("ghost")).toBeNull();
  });

  test("a failing CLI surfaces an install hint", async () => {
    const run = async () => {
      throw new Error("aria-memo exited with 127: not found");
    };
    const store = new LocalMemoryStore({ exec: run });
    await expect(store.put("k", "v")).rejects.toThrow(/aria-memo/);
  });

  test("encode/decode roundtrip", () => {
    expect(encodeLocal("k", "v")).toBe("k: v");
    expect(decodeLocal("k", "k: v")).toBe("v");
    expect(decodeLocal("k", "other")).toBe("other");
  });
});

describe("both backend", () => {
  test("writes to local and cloud, cloud wins on read", async () => {
    const localWrites: string[] = [];
    const cloudWrites: string[] = [];
    const local: MemoryStore = {
      backend: "local",
      put: async (_k, v) => void localWrites.push(v),
      get: async () => "from-local",
    };
    const cloud: MemoryStore = {
      backend: "cloud",
      put: async (_k, v) => void cloudWrites.push(v),
      get: async () => "from-cloud",
    };
    const composite = new CompositeMemoryStore(local, cloud);
    expect(composite.backend).toBe("both" as MemoryBackend);
    await composite.put("k", "v");
    expect(localWrites).toEqual(["v"]);
    expect(cloudWrites).toEqual(["v"]);
    expect(await composite.get("k")).toBe("from-cloud");
  });

  test("falls back to local when the cloud read fails", async () => {
    const composite = new CompositeMemoryStore(
      { backend: "local", put: async () => {}, get: async () => "from-local" },
      {
        backend: "cloud",
        put: async () => {},
        get: async () => {
          throw new Error("cloud down");
        },
      },
    );
    expect(await composite.get("k")).toBe("from-local");
  });

  test("tolerates one failing write", async () => {
    const composite = new CompositeMemoryStore(
      {
        backend: "local",
        put: async () => {
          throw new Error("disk full");
        },
        get: async () => null,
      },
      { backend: "cloud", put: async () => {}, get: async () => null },
    );
    await composite.put("k", "v");
  });

  test("errors only when every write fails", async () => {
    const failing: MemoryStore = {
      backend: "cloud",
      put: async () => {
        throw new Error("nope");
      },
      get: async () => {
        throw new Error("nope");
      },
    };
    const composite = new CompositeMemoryStore(failing, failing);
    await expect(composite.put("k", "v")).rejects.toThrow(/every backend/);
    await expect(composite.get("k")).rejects.toThrow(/every backend/);
  });
});

describe("Session backend selection", () => {
  test("defaults to cloud and honours the backend override", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse({ key: "k", value: "cloud-value" }));
    const session = new Session("sess_1", { ...client, fetch: impl });
    expect(session.backend).toBe("cloud");

    await session.memorize("k", "v");
    expect(calls[0].url).toBe("http://aria.test/v1/agents/sessions/sess_1/memory");

    // Explicit local override goes through the aria-memo CLI, not HTTP.
    const { run, calls: cliCalls } = fakeExec(() => JSON.stringify([{ content: "k: local-value" }]));
    const localSession = new Session("sess_1", {
      ...client,
      fetch: impl,
      backend: "local",
      exec: run,
    });
    expect(localSession.backend).toBe("local");
    expect(await localSession.recall("k")).toBe("local-value");
    expect(cliCalls[0][0]).toBe("--db");
  });

  test("both backend writes local then cloud", async () => {
    const { impl, calls } = fakeFetch(() => jsonResponse({ key: "k", value: "v" }));
    const { run, calls: cliCalls } = fakeExec(() => "");
    const session = new Session("sess_1", {
      ...client,
      fetch: impl,
      backend: "both",
      exec: run,
    });
    await session.memorize("k", "v");
    expect(cliCalls.length).toBe(1);
    expect(calls.some((c) => c.url.endsWith("/memory"))).toBe(true);
  });

  test("unknown backend override is rejected", async () => {
    const session = new Session("sess_1", client);
    // @ts-expect-error runtime guard check
    await expect(session.recall("k", { backend: "nonsense" })).rejects.toThrow(
      /unknown memory backend/,
    );
  });

  test("Session.create inherits the agent memory config", async () => {
    const { impl } = fakeFetch((url: string) => {
      if (url.endsWith("/v1/agents/sessions")) return jsonResponse({ id: "sess_9" });
      return jsonResponse({ key: "k", value: "v" });
    });
    const agent = new Agent({
      name: "tutor",
      client: { ...client, fetch: impl },
      memory: { backend: "both", memoDb: "/tmp/m.db" },
    });
    const session = await Session.create(agent);
    expect(session.id).toBe("sess_9");
    expect(session.backend).toBe("both");
  });
});
