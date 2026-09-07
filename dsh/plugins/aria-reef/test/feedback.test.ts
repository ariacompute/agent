import { describe, it, expect } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import {
  applyRubric,
  createReportTool,
  createStatusTool,
  defaultRubric,
  feedbackFromOutcome,
  isEligible,
  meanScore,
  normalizeScore,
  summarizeFeedback,
} from "../src/feedback.ts";
import { createMemoryStore } from "../src/store.ts";
import { makeFeedback, makeRecord } from "./helpers.ts";

describe("score normalization", () => {
  it("accepts 0..1 and normalizes 1..5", () => {
    expect(normalizeScore(0.2)).toBeCloseTo(0.2);
    expect(normalizeScore(1)).toBe(1);
    expect(normalizeScore(5)).toBe(1);
    expect(normalizeScore(4)).toBeCloseTo(0.8);
    expect(normalizeScore(-3)).toBe(0);
    expect(normalizeScore(99)).toBe(1);
  });

  it("fails loudly on non-finite scores", () => {
    expect(() => normalizeScore(Number.NaN)).toThrow(/finite/);
    try {
      normalizeScore(Number.POSITIVE_INFINITY);
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.INVALID_PARAM);
    }
  });
});

describe("feedback sources", () => {
  it("maps task outcomes to scores", () => {
    const store = createMemoryStore();
    const success = feedbackFromOutcome({ store }, "r1", "success");
    const failure = feedbackFromOutcome({ store }, "r1", "failure");
    const unknown = feedbackFromOutcome({ store }, "r1", "unknown");
    expect(success.score).toBe(1);
    expect(failure.score).toBe(0);
    expect(unknown.score).toBeUndefined();
    expect(unknown.structured).toEqual({ outcome: "unknown" });
  });

  it("computes mean scores only over scored feedback", () => {
    expect(meanScore([makeFeedback({ score: 0.2 }), makeFeedback({ score: 0.6 })])).toBeCloseTo(0.4);
    expect(meanScore([makeFeedback({ text: "no score" })])).toBeUndefined();
  });
});

describe("eligibility", () => {
  it("requires enough feedback and at least one score", () => {
    const record = makeRecord({ recordId: "r1" });
    const scored = makeFeedback({ recordId: "r1", score: 0.1 });
    expect(isEligible(record, [scored])).toBe(true);
    expect(isEligible(record, [], { minFeedback: 1 })).toBe(false);
    expect(isEligible(record, [makeFeedback({ recordId: "r1", text: "meh" })])).toBe(false);
    expect(
      isEligible(record, [makeFeedback({ recordId: "r2", score: 0.1 })], { minFeedback: 1 }),
    ).toBe(false);
    expect(isEligible(makeRecord({ recordId: "r1", trainedAt: "t" }), [scored])).toBe(false);
    expect(isEligible(record, [scored], { minFeedback: 2 })).toBe(false);
  });
});

describe("default rubric", () => {
  it("returns null without a response and scores outcomes otherwise", () => {
    expect(defaultRubric(makeRecord())).toBeNull();
    expect(defaultRubric(makeRecord({ response: "ok", outcome: "success" }))).toBeCloseTo(0.8);
    expect(defaultRubric(makeRecord({ response: "ok", outcome: "failure" }))).toBeCloseTo(0.1);
    expect(defaultRubric(makeRecord({ response: "ok" }))).toBeCloseTo(0.5);
    expect(defaultRubric(makeRecord({ response: "tool error", outcome: "success" }))).toBeCloseTo(0.6);
    expect(defaultRubric(makeRecord({ response: {} }))).toBeNull();
  });

  it("persists one rubric score per untrained record", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1", response: "ok", outcome: "failure" }));
    await store.records.append(makeRecord({ recordId: "r2", trainedAt: "x", response: "ok" }));
    const first = await applyRubric({ store });
    expect(first.length).toBe(1);
    expect(first[0]?.source).toBe("rubric");
    expect(await applyRubric({ store })).toEqual([]);
  });
});

describe("summarizeFeedback", () => {
  it("renders scores, prompts and feedback text", () => {
    const records = [makeRecord({ recordId: "r1", request: { prompt: "do the thing" } })];
    const feedback = [makeFeedback({ recordId: "r1", score: 0.25, text: "too slow" })];
    const summary = summarizeFeedback(records, feedback);
    expect(summary).toContain("record r1");
    expect(summary).toContain("score=0.25");
    expect(summary).toContain("do the thing");
    expect(summary).toContain("too slow");
    expect(summarizeFeedback(records, [])).toBe("");
  });
});

describe("aria_reef_report tool", () => {
  it("stores user feedback and reports eligibility", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1" }));
    const tool = createReportTool({ store, lastRecordId: () => "r1", policy: { minFeedback: 1 } });
    const result = await tool.execute({ score: 4, text: "good", structured: '{"a":1}' });
    expect(result.recordId).toBe("r1");
    expect(result.score).toBeCloseTo(0.8);
    expect(result.eligible).toBe(true);
    const stored = await store.feedback.forRecord("r1");
    expect(stored[0]?.source).toBe("user");
    expect(stored[0]?.structured).toEqual({ a: 1 });
  });

  it("falls back to the latest record id", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r9" }));
    const tool = createReportTool({ store, lastRecordId: () => "r9" });
    const result = await tool.execute({ text: "ok" });
    expect(result.recordId).toBe("r9");
    expect(result.score).toBeUndefined();
  });

  it("rejects empty feedback", async () => {
    const store = createMemoryStore();
    const tool = createReportTool({ store, lastRecordId: () => "r1" });
    await expect(tool.execute({})).rejects.toThrow(/at least one of score/);
  });

  it("rejects unknown record ids", async () => {
    const store = createMemoryStore();
    const tool = createReportTool({ store });
    await expect(tool.execute({ record_id: "nope", score: 1 })).rejects.toThrow(/unknown record_id/);
  });

  it("rejects malformed structured payloads and missing record context", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1" }));
    const tool = createReportTool({ store });
    await expect(tool.execute({ record_id: "r1", structured: "[1,2]" })).rejects.toThrow(/JSON object/);
    await expect(tool.execute({ score: 1 })).rejects.toThrow(/no recent turn/);
  });
});

describe("aria_reef_status tool", () => {
  it("reports counters", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1" }));
    await store.records.append(makeRecord({ recordId: "r2", trainedAt: "t" }));
    await store.feedback.append(makeFeedback({ recordId: "r1", score: 1 }));
    const tool = createStatusTool({ store, release: "v7", recipes: ["prompt"] });
    const status = await tool.execute();
    expect(status).toEqual({
      release: "v7",
      records: 2,
      pending: 1,
      feedback: 1,
      recipes: ["prompt"],
      lastRecordId: expect.any(String),
    });
  });
});
