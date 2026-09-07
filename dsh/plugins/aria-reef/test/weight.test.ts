import { describe, it, expect } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import { createFakeAriapinClient } from "../src/ariapin-client.ts";
import {
  collectRollouts,
  createWeightRecipe,
  rolloutsToJsonl,
} from "../src/recipes/weight.ts";
import { makeFeedback, makeRecord } from "./helpers.ts";

function scoredRecords(count: number, score: number) {
  const records = [];
  const feedback = [];
  for (let i = 0; i < count; i += 1) {
    const recordId = `r${i}`;
    records.push(
      makeRecord({
        recordId,
        request: { prompt: `prompt ${i}` },
        response: { text: `answer ${i}` },
      }),
    );
    feedback.push(makeFeedback({ recordId, score }));
  }
  return { records, feedback };
}

describe("rollout collection", () => {
  it("keeps only rollouts above the score gate", () => {
    const { records, feedback } = scoredRecords(3, 0.9);
    const low = makeRecord({ recordId: "bad", request: { prompt: "p" }, response: "a" });
    const all = collectRollouts([...records, low], [...feedback, makeFeedback({ recordId: "bad", score: 0.1 })], 0.6);
    expect(all.length).toBe(3);
    expect(all[0]?.prompt).toBe("prompt 0");
    expect(all[0]?.completion).toBe('{"text":"answer 0"}');
    expect(all.every((rollout) => rollout.score >= 0.6)).toBe(true);
  });

  it("serializes rollouts as jsonl", () => {
    const jsonl = rolloutsToJsonl([
      { recordId: "r1", prompt: "p", completion: "c", score: 0.75 },
    ]);
    expect(JSON.parse(jsonl)).toEqual({ prompt: "p", completion: "c", score: 0.75, record_id: "r1" });
  });
});

describe("weight recipe", () => {
  it("gates on the number of high-scoring rollouts", () => {
    const { client } = createFakeAriapinClient();
    const recipe = createWeightRecipe({ baseModel: "base/model", minRollouts: 4 }, { client });
    const few = scoredRecords(2, 0.9);
    expect(recipe.eligible(few.records, few.feedback)).toBe(false);
    const many = scoredRecords(4, 0.9);
    expect(recipe.eligible(many.records, many.feedback)).toBe(true);
    const weak = scoredRecords(4, 0.2);
    expect(recipe.eligible(weak.records, weak.feedback)).toBe(false);
  });

  it("dispatches a dataset + training job through ariapin", async () => {
    const { client, state } = createFakeAriapinClient();
    const recipe = createWeightRecipe(
      { baseModel: "base/model", jobType: "sft", minRollouts: 2 },
      { client },
    );
    const { records, feedback } = scoredRecords(2, 0.9);
    const candidate = await recipe.propose({ records, feedback, current: "" });

    expect(state.datasets.length).toBe(1);
    expect(state.datasets[0]?.name).toBe("reef-weights-active");
    expect(state.jobs.length).toBe(1);
    expect(state.started).toEqual(["jb_2"]);
    expect(candidate.recipeKind).toBe("weight");
    expect(candidate.kind).toBe("weight");
    const meta = candidate.meta as { jobId: string; datasetId: string; rollouts: number; baseModel: string };
    expect(meta.jobId).toBe("jb_2");
    expect(meta.datasetId).toBe("ds_1");
    expect(meta.rollouts).toBe(2);
    expect(meta.baseModel).toBe("base/model");
    expect(candidate.content).toContain("jb_2");
    expect(candidate.diff).toContain("--- a/weight/weights/active");
  });

  it("can wait for the job and records the produced model id", async () => {
    const { client, state } = createFakeAriapinClient({ modelId: "mdl_1" });
    const recipe = createWeightRecipe(
      { baseModel: "base/model", minRollouts: 1, waitForJob: true },
      { client },
    );
    const { records, feedback } = scoredRecords(1, 1);
    const candidate = await recipe.propose({ records, feedback, current: "" });
    expect(candidate.meta?.modelId).toBe("mdl_1");
    expect(candidate.meta?.status).toBe("succeeded");
    expect(state.jobs.length).toBe(1);
  });

  it("records the serving agent id in the candidate meta", async () => {
    const { client } = createFakeAriapinClient();
    const recipe = createWeightRecipe(
      { baseModel: "base/model", minRollouts: 1, agentId: "agt_1" },
      { client },
    );
    const { records, feedback } = scoredRecords(1, 0.9);
    const candidate = await recipe.propose({ records, feedback, current: "" });
    expect(candidate.meta?.agentId).toBe("agt_1");
    expect(candidate.content).toContain("agt_1");
  });

  it("fails loudly when the job does not succeed or rollouts are missing", async () => {
    const { client } = createFakeAriapinClient({ jobStatus: "failed" });
    const recipe = createWeightRecipe(
      { baseModel: "base/model", minRollouts: 1, waitForJob: true },
      { client },
    );
    const { records, feedback } = scoredRecords(1, 1);
    await expect(recipe.propose({ records, feedback, current: "" })).rejects.toThrow(/ended with status failed/);
    try {
      await recipe.propose({ records, feedback, current: "" });
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.REEF_TRAIN);
    }

    await expect(recipe.propose({ records: [], feedback: [], current: "" })).rejects.toThrow(
      /need 1 rollouts/,
    );
  });
});
