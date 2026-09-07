import { describe, it, expect } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import {
  defaultRunner,
  evaluateCandidate,
  keywords,
  tasksFromRecords,
} from "../src/evaluate.ts";
import type { Candidate } from "../src/types.ts";
import { makeFeedback, makeRecord } from "./helpers.ts";

function candidate(content: string, patch: Partial<Candidate> = {}): Candidate {
  return {
    recipe: "skillclaw",
    recipeKind: "harness",
    kind: "skill",
    target: "skills/agent",
    content,
    diff: "",
    ...patch,
  };
}

async function score(content: string, task: { id: string; input: string; hints?: string[] }): Promise<number> {
  return Number(await defaultRunner({ content, task, kind: "skill" }));
}

describe("defaultRunner", () => {
  it("scores empty content as zero", async () => {
    expect(await score("   ", { id: "t", input: "" })).toBe(0);
  });

  it("rewards hint coverage", async () => {
    const task = { id: "t", input: "", hints: ["citations", "tests"] };
    const none = await score("skill body", task);
    const half = await score("skill body with citations", task);
    const all = await score("citations and tests", task);
    expect(half).toBeGreaterThan(none);
    expect(all).toBeGreaterThan(half);
    expect(all).toBeCloseTo(1);
  });

  it("penalizes placeholder markers", async () => {
    const task = { id: "t", input: "" };
    const clean = await score("real content", task);
    const dirty = await score("real content TODO", task);
    expect(dirty).toBeLessThan(clean);
  });
});

describe("task derivation", () => {
  it("derives hints from feedback text", () => {
    const records = [makeRecord({ recordId: "r1", request: { prompt: "summarize" } })];
    const feedback = [makeFeedback({ recordId: "r1", text: "always include citations" })];
    const tasks = tasksFromRecords(records, feedback);
    expect(tasks.length).toBe(1);
    expect(tasks[0]?.id).toBe("r1");
    expect(tasks[0]?.input).toBe("summarize");
    expect(tasks[0]?.hints).toContain("citations");
    expect(tasks[0]?.hints).not.toContain("always");
  });

  it("falls back to a default task when there is no feedback", () => {
    const tasks = tasksFromRecords([makeRecord({ recordId: "r1" })], []);
    expect(tasks).toEqual([{ id: "default", input: "" }]);
  });

  it("extracts distinctive keywords", () => {
    expect(keywords("Please always include citations")).toEqual(["include", "citations"]);
    expect(keywords("")).toEqual([]);
  });
});

describe("evaluateCandidate", () => {
  const tasks = [{ id: "t1", input: "", hints: ["citations"] }];

  it("keeps the winner and prefers current on ties", async () => {
    const better = await evaluateCandidate({ current: "", candidate: candidate("citations matter"), tasks });
    expect(better.winner).toBe("candidate");
    expect(better.current).toBe(0);

    const tie = await evaluateCandidate({ current: "citations", candidate: candidate("citations"), tasks });
    expect(tie.winner).toBe("current");
    expect(tie.current).toBe(tie.candidate);
  });

  it("honours minImprovement", async () => {
    const result = await evaluateCandidate({
      current: "citations",
      candidate: candidate("citations"),
      tasks,
      minImprovement: 0.1,
    });
    expect(result.winner).toBe("current");
  });

  it("reports per-task scores", async () => {
    const result = await evaluateCandidate({
      current: "",
      candidate: candidate("citations"),
      tasks: [tasks[0]!, { id: "t2", input: "" }],
    });
    expect(result.currentScores.map((s) => s.taskId)).toEqual(["t1", "t2"]);
    expect(result.candidateScores.length).toBe(2);
  });

  it("fails loudly on broken runners", async () => {
    await expect(
      evaluateCandidate({
        current: "",
        candidate: candidate("x"),
        tasks,
        runner: () => Number.NaN,
      }),
    ).rejects.toThrow(/non-finite/);

    await expect(
      evaluateCandidate({
        current: "",
        candidate: candidate("x"),
        tasks,
        runner: () => {
          throw new Error("runner exploded");
        },
      }),
    ).rejects.toThrow(/runner failed/);

    try {
      await evaluateCandidate({
        current: "",
        candidate: candidate("x"),
        tasks,
        runner: () => Number.NaN,
      });
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.INVALID_PARAM);
    }
  });
});
