import { describe, it, expect } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import {
  attachReceipt,
  createRecorder,
  outcomeFromResponse,
  promptOf,
  recordIdOf,
  registerServeHooks,
} from "../src/record.ts";
import { createMemoryStore, type ReefStore } from "../src/store.ts";
import type { AgentRecord } from "../src/types.ts";
import { createFakeCtx } from "./helpers.ts";

function throwingStore(): { store: ReefStore; calls: number } {
  const inner = createMemoryStore();
  const state = { calls: 0 };
  return {
    calls: state.calls,
    store: {
      records: {
        async append() {
          state.calls += 1;
          throw new Error("disk on fire");
        },
        get: (id) => inner.records.get(id),
        update: (id, patch) => inner.records.update(id, patch),
        list: (opts) => inner.records.list(opts),
      },
      feedback: inner.feedback,
    },
  };
}

describe("record helpers", () => {
  it("extracts the user prompt from a pre-step payload", () => {
    expect(promptOf({ prompt: "hi" })).toBe("hi");
    expect(promptOf({ messages: [{ role: "system", content: "s" }, { role: "user", content: "yo" }] })).toBe("yo");
    expect(promptOf({ messages: [{ role: "user", content: [{ type: "text", text: "block" }] }] })).toBe("block");
    expect(promptOf("plain")).toBe("plain");
    expect(promptOf(undefined)).toBe("");
  });

  it("infers outcomes from post-step payloads", () => {
    expect(outcomeFromResponse({ finish: { reason: { kind: "stop" } } })).toBe("success");
    expect(outcomeFromResponse({ finish: { reason: { kind: "error" } } })).toBe("failure");
    expect(outcomeFromResponse({ error: "boom" })).toBe("failure");
    expect(outcomeFromResponse({ outcome: "failure" })).toBe("failure");
    expect(outcomeFromResponse({})).toBe("unknown");
  });

  it("attaches receipts without dropping fields", () => {
    const withReef = attachReceipt({ messages: [], reef: { other: 1 } }, "rec_1") as {
      reef: { recordId: string; other: number };
    };
    expect(withReef.reef).toEqual({ other: 1, recordId: "rec_1" });
    expect(attachReceipt("text", "rec_1")).toBe("text");
    expect(recordIdOf({ reef: { recordId: "rec_2" } })).toBe("rec_2");
    expect(recordIdOf({ recordId: "rec_3" })).toBe("rec_3");
    expect(recordIdOf({})).toBeUndefined();
  });
});

describe("recorder", () => {
  it("persists asynchronously and tracks the last record id", async () => {
    const store = createMemoryStore();
    const recorder = createRecorder({ store, release: "v1" });
    const record = recorder.start({ prompt: "hello" });
    expect(recorder.lastRecordId()).toBe(record.recordId);
    await recorder.flush();
    const stored = await store.records.get(record.recordId);
    expect(stored?.release).toBe("v1");
    expect(stored?.request).toEqual({ prompt: "hello" });
  });

  it("never throws into the agent loop when persistence fails", async () => {
    const { store } = throwingStore();
    const logged: string[] = [];
    const recorder = createRecorder({ store, release: "v1", logger: (m) => logged.push(m) });
    const record = recorder.start({ prompt: "hello" });
    await recorder.flush();
    expect(logged.some((line) => line.includes("disk on fire"))).toBe(true);
    expect(record.recordId).toBeTruthy();
  });

  it("completes records and reports unknown ids", async () => {
    const store = createMemoryStore();
    const logged: string[] = [];
    const recorder = createRecorder({ store, release: "v1", logger: (m) => logged.push(m) });
    const record = recorder.start({ prompt: "hello" });
    await recorder.flush();
    const updated = await recorder.complete(record.recordId, { finish: { reason: { kind: "stop" } } });
    expect(updated?.outcome ?? "").toBe("success");
    expect(await recorder.complete("rec_missing", "x")).toBeNull();
    expect(logged.some((line) => line.includes("not found"))).toBe(true);
  });
});

describe("serve hooks", () => {
  it("issues a record_id per turn and always calls next", async () => {
    const store = createMemoryStore();
    const recorder = createRecorder({ store, release: "v1" });
    const fake = createFakeCtx();
    registerServeHooks(fake.ctx, recorder);
    expect(fake.listeners.has("agent/pre-step")).toBe(true);
    expect(fake.listeners.has("agent/post-step")).toBe(true);

    const seen: unknown[] = [];
    const forwarded = await fake.emit(
      "agent/pre-step",
      { messages: [{ role: "user", content: "hi" }] },
      (value?: unknown) => {
        seen.push(value);
        return "next-called";
      },
    );
    expect(forwarded).toBe("next-called");
    const payload = seen[0] as { reef: { recordId: string }; messages: unknown[] };
    const last = recorder.lastRecordId();
    expect(last).toBeTruthy();
    expect(payload.reef.recordId).toBe(last ?? "");
    expect(payload.messages.length).toBe(1);

    await recorder.flush();
    expect((await store.records.list()).length).toBe(1);

    await fake.emit(
      "agent/post-step",
      { reef: { recordId: payload.reef.recordId }, finish: { reason: { kind: "error" } } },
      (value?: unknown) => value,
    );
    await recorder.flush();
    const stored = await store.records.get(payload.reef.recordId);
    expect(stored?.outcome ?? "").toBe("failure");
  });

  it("does not wait for the outcome write before calling next", async () => {
    const inner = createMemoryStore();
    let release!: () => void;
    const gate = new Promise<AgentRecord | null>((resolve) => {
      release = () => resolve(null);
    });
    const store: ReefStore = {
      records: { ...inner.records, update: () => gate },
      feedback: inner.feedback,
    };
    const recorder = createRecorder({ store, release: "v1", logger: () => undefined });
    const record = recorder.start({ prompt: "x" });
    await recorder.flush();

    const fake = createFakeCtx();
    registerServeHooks(fake.ctx, recorder);
    let nextCalled = false;
    const forwarded = await fake.emit(
      "agent/post-step",
      { reef: { recordId: record.recordId } },
      () => {
        nextCalled = true;
        return "done";
      },
    );
    expect(forwarded).toBe("done");
    expect(nextCalled).toBe(true);

    release();
    await recorder.flush();
  });

  it("calls next even when record creation fails", async () => {
    const { store } = throwingStore();
    const recorder = createRecorder({ store, release: "v1", logger: () => undefined });
    const fake = createFakeCtx();
    registerServeHooks(fake.ctx, recorder);
    const forwarded = (await fake.emit("agent/pre-step", { prompt: "x" }, (value?: unknown) => value)) as {
      prompt: string;
      reef: { recordId: string };
    };
    expect(forwarded.prompt).toBe("x");
    expect(forwarded.reef.recordId).toBeTruthy();
  });

  it("returns the value when no next() is provided", async () => {
    const store = createMemoryStore();
    const recorder = createRecorder({ store, release: "v1" });
    const fake = createFakeCtx();
    registerServeHooks(fake.ctx, recorder);
    const forwarded = (await fake.emit("agent/pre-step", { prompt: "x" })) as { reef: { recordId: string } };
    const last = recorder.lastRecordId();
    expect(last).toBeTruthy();
    expect(forwarded.reef.recordId).toBe(last ?? "");
  });
});

describe("error codes", () => {
  it("exposes reef codes on the shared error catalogue", () => {
    expect(ErrorCode.REEF).toBe("REEF");
    expect(ErrorCode.REEF_STORE).toBe("REEF_STORE");
    expect(ErrorCode.REEF_TRAIN).toBe("REEF_TRAIN");
    expect(ErrorCode.REEF_GIT).toBe("REEF_GIT");
    expect(ErrorCode.ARIAPIN).toBe("ARIAPIN");
  });
});
