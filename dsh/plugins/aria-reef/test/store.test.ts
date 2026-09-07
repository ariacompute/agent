import { describe, it, expect } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ErrorCode } from "../../shared/src/error.ts";
import { createFileStore, createMemoryStore } from "../src/store.ts";
import { makeFeedback, makeRecord } from "./helpers.ts";

describe("memory store", () => {
  it("appends, reads and updates records", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1" }));
    await store.records.append(makeRecord({ recordId: "r2" }));
    expect((await store.records.get("r1"))?.recordId).toBe("r1");
    const updated = await store.records.update("r1", { outcome: "success", trainedAt: "now" });
    expect(updated?.outcome).toBe("success");
    expect((await store.records.get("r1"))?.trainedAt).toBe("now");
    expect(await store.records.update("missing", { outcome: "failure" })).toBeNull();
  });

  it("filters untrained records and applies limits", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1" }));
    await store.records.append(makeRecord({ recordId: "r2", trainedAt: "t" }));
    await store.records.append(makeRecord({ recordId: "r3" }));
    expect((await store.records.list()).length).toBe(3);
    const pending = await store.records.list({ untrainedOnly: true });
    expect(pending.map((r) => r.recordId).sort()).toEqual(["r1", "r3"]);
    expect((await store.records.list({ limit: 1 })).length).toBe(1);
  });

  it("filters feedback by record", async () => {
    const store = createMemoryStore();
    await store.feedback.append(makeFeedback({ feedbackId: "f1", recordId: "r1", score: 0.2 }));
    await store.feedback.append(makeFeedback({ feedbackId: "f2", recordId: "r2", score: 0.9 }));
    expect((await store.feedback.forRecord("r1")).length).toBe(1);
    expect((await store.feedback.forRecords(["r1", "r2"])).length).toBe(2);
    expect((await store.feedback.forRecords(["r2"]))[0]?.score).toBe(0.9);
    expect((await store.feedback.list({ limit: 1 })).length).toBe(1);
  });
});

describe("file store", () => {
  function tmp(): string {
    return mkdtempSync(join(tmpdir(), "reef-store-"));
  }

  it("round-trips records and feedback as JSONL", async () => {
    const dir = tmp();
    try {
      const store = createFileStore(dir);
      await store.records.append(makeRecord({ recordId: "r1", request: { prompt: "hi" } }));
      await store.records.append(makeRecord({ recordId: "r2", trainedAt: "later" }));
      await store.feedback.append(makeFeedback({ recordId: "r1", score: 0.5 }));

      const reopened = createFileStore(dir);
      expect((await reopened.records.get("r1"))?.request).toEqual({ prompt: "hi" });
      expect((await reopened.records.list({ untrainedOnly: true })).map((r) => r.recordId)).toEqual(["r1"]);
      expect((await reopened.feedback.forRecord("r1")).length).toBe(1);

      const updated = await reopened.records.update("r1", { outcome: "failure" });
      expect(updated?.outcome).toBe("failure");
      expect((await reopened.records.list()).length).toBe(2);
      expect(await reopened.records.update("nope", { outcome: "failure" })).toBeNull();
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  it("fails loudly on corrupt jsonl instead of skipping", async () => {
    const dir = tmp();
    try {
      writeFileSync(join(dir, "records.jsonl"), '{"recordId":"r1"}\nnot-json\n', "utf8");
      const store = createFileStore(dir);
      await expect(store.records.list()).rejects.toThrow(/corrupt line 2/);
      try {
        await store.records.list();
      } catch (err) {
        expect((err as { code?: string }).code).toBe(ErrorCode.REEF_STORE);
      }
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });
});
