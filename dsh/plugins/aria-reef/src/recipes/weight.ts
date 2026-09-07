import { AriaError, ErrorCode } from "../../../shared/src/error.ts";
import type { AriapinClient } from "../ariapin-client.ts";
import { unifiedDiff } from "../diff.ts";
import { meanScore } from "../feedback.ts";
import { promptOf } from "../record.ts";
import type { AgentRecord, Candidate, Feedback } from "../types.ts";
import type { Recipe } from "./recipe.ts";

export const DEFAULT_WEIGHT_TARGET = "weights/active";

export interface WeightRecipeOptions {
  name?: string;
  /** Artifact name inside the artifact repo. */
  target?: string;
  /** Minimum number of rollouts above `minScore` required to train. */
  minRollouts?: number;
  /** Only rollouts with a mean score >= this are used as training data. */
  minScore?: number;
  /** Base model id passed to ariapin (must be in its allow-list). */
  baseModel: string;
  /** Job type: sft / dpo / grpo / ... (default sft). */
  jobType?: string;
  lora?: Record<string, unknown>;
  hyperparams?: Record<string, unknown>;
  datasetName?: string;
  datasetFormat?: string;
  /**
   * Agent serving this model. Recorded in the candidate meta so a later reload
   * can fall back to restarting the agent when the reload endpoint is missing.
   */
  agentId?: string;
  /** Call `POST /v1/jobs/{id}/start` right after creation. */
  autoStart?: boolean;
  /** Block until the job reaches a terminal state (usually too slow online). */
  waitForJob?: boolean;
}

export interface Rollout {
  recordId: string;
  prompt: string;
  completion: string;
  score: number;
}

function responseText(response: unknown): string {
  if (response === undefined || response === null) {
    return "";
  }
  if (typeof response === "string") {
    return response;
  }
  try {
    return JSON.stringify(response);
  } catch {
    return String(response);
  }
}

/** Collect scored rollouts that pass the quality gate. */
export function collectRollouts(
  records: AgentRecord[],
  feedback: Feedback[],
  minScore: number,
): Rollout[] {
  const byRecord = new Map<string, Feedback[]>();
  for (const item of feedback) {
    const list = byRecord.get(item.recordId) ?? [];
    list.push(item);
    byRecord.set(item.recordId, list);
  }
  const rollouts: Rollout[] = [];
  for (const record of records) {
    const items = byRecord.get(record.recordId) ?? [];
    const score = meanScore(items);
    if (score === undefined || score < minScore) {
      continue;
    }
    rollouts.push({
      recordId: record.recordId,
      prompt: promptOf(record.request),
      completion: responseText(record.response),
      score,
    });
  }
  return rollouts;
}

export function rolloutsToJsonl(rollouts: Rollout[]): string {
  return rollouts
    .map((rollout) =>
      JSON.stringify({
        prompt: rollout.prompt,
        completion: rollout.completion,
        score: Number(rollout.score.toFixed(4)),
        record_id: rollout.recordId,
      }),
    )
    .join("\n");
}

/**
 * Weight recipe: aggregate scored rollouts into a dataset and dispatch a
 * training job through ariapin (Slime training + SGLang serving on that side).
 */
export function createWeightRecipe(
  options: WeightRecipeOptions,
  deps: { client: AriapinClient },
): Recipe {
  const name = options.name ?? "weight";
  const target = options.target ?? DEFAULT_WEIGHT_TARGET;
  const minRollouts = options.minRollouts ?? 4;
  const minScore = options.minScore ?? 0.6;

  return {
    name,
    kind: "weight",
    artifact: "weight",
    target,
    eligible(records, feedback) {
      return collectRollouts(records, feedback, minScore).length >= minRollouts;
    },
    async propose(input) {
      const rollouts = collectRollouts(input.records, input.feedback, minScore);
      if (rollouts.length < minRollouts) {
        throw new AriaError(
          `recipe ${name}: need ${minRollouts} rollouts with score >= ${minScore}, got ${rollouts.length}`,
          ErrorCode.REEF_TRAIN,
        );
      }
      const dataset = await deps.client.createDataset({
        name: options.datasetName ?? `reef-${target.replace(/[^A-Za-z0-9._-]+/g, "-")}`,
        format: options.datasetFormat ?? "sft",
        content: rolloutsToJsonl(rollouts),
      });
      const job = await deps.client.createJob({
        baseModel: options.baseModel,
        type: options.jobType ?? "sft",
        datasetId: dataset.datasetId,
        lora: options.lora,
        hyperparams: options.hyperparams,
      });
      if (options.autoStart !== false) {
        await deps.client.startJob(job.jobId);
      }

      let status: string | undefined;
      let modelId: string | undefined;
      if (options.waitForJob) {
        const terminal = await deps.client.waitForJob(job.jobId);
        status = terminal.status;
        modelId = terminal.modelId;
        if (terminal.status !== "succeeded") {
          throw new AriaError(
            `recipe ${name}: training job ${job.jobId} ended with status ${terminal.status}`,
            ErrorCode.REEF_TRAIN,
          );
        }
      }

      const meta: Record<string, unknown> = {
        jobId: job.jobId,
        datasetId: dataset.datasetId,
        baseModel: options.baseModel,
        rollouts: rollouts.length,
      };
      if (status) {
        meta.status = status;
      }
      if (modelId) {
        meta.modelId = modelId;
      }
      if (options.agentId) {
        meta.agentId = options.agentId;
      }

      const content = JSON.stringify(meta, null, 2);
      const candidate: Candidate = {
        recipe: name,
        recipeKind: "weight",
        kind: "weight",
        target,
        content,
        diff: unifiedDiff(input.current, content, `weight/${target}`),
        notes: `${rollouts.length} rollout(s) with score >= ${minScore}`,
        meta,
      };
      const mean = meanScore(input.feedback);
      if (mean !== undefined) {
        candidate.score = mean;
      }
      return candidate;
    },
  };
}
