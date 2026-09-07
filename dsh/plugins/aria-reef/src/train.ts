import type { ArtifactRepo } from "./artifact.ts";
import type { ReefConfig } from "./config.ts";
import { applyRubric, defaultRubric, isEligible, type Rubric } from "./feedback.ts";
import { evaluateCandidate, tasksFromRecords, type EvalRunner } from "./evaluate.ts";
import type { ReefStore } from "./store.ts";
import type { Surface } from "./surface.ts";
import type { ArtifactRef, CycleReport, RecipeCycleResult } from "./types.ts";
import type { LlmProposer } from "./recipes/llm.ts";
import type { RecipeRegistry } from "./recipes/recipe.ts";

export interface TrainDeps {
  store: ReefStore;
  artifact: ArtifactRepo;
  surface: Surface;
  registry: RecipeRegistry;
  config: ReefConfig;
  /** Proposer shared by harness recipes. */
  llm?: LlmProposer;
  runner?: EvalRunner;
  /** Relative improvement required before a candidate is accepted. */
  minImprovement?: number;
  rubric?: Rubric;
  now?: () => Date;
  logger?: (message: string) => void;
}

function errorText(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

/**
 * One full Grow -> Evaluate -> Commit -> Surface pass.
 *
 * Records are consumed (marked `trainedAt`) once a recipe produced a candidate,
 * so a failing evaluation cannot loop forever on the same batch.
 */
export async function runCycle(deps: TrainDeps): Promise<CycleReport> {
  const now = deps.now ?? (() => new Date());
  const logger = deps.logger ?? ((message: string) => console.warn(`[aria-reef] ${message}`));
  const report: CycleReport = { release: deps.config.release, scanned: 0, results: [] };

  if (deps.config.rubricEnabled) {
    await applyRubric({ store: deps.store }, deps.rubric ?? defaultRubric, {
      limit: deps.config.batchSize,
    });
  }

  const records = await deps.store.records.list({
    untrainedOnly: true,
    limit: deps.config.batchSize,
  });
  report.scanned = records.length;
  if (records.length === 0) {
    return report;
  }

  const feedback = await deps.store.feedback.forRecords(records.map((record) => record.recordId));
  const eligibleRecords = records.filter((record) =>
    isEligible(record, feedback, { minFeedback: deps.config.minFeedback }),
  );
  if (eligibleRecords.length === 0) {
    logger(`no eligible records (${records.length} scanned, minFeedback=${deps.config.minFeedback})`);
    return report;
  }

  for (const recipe of deps.registry.list()) {
    if (!deps.config.recipes.includes(recipe.name)) {
      report.results.push({
        recipe: recipe.name,
        kind: recipe.kind,
        target: recipe.target,
        eligible: false,
        skipReason: "recipe disabled by configuration",
        applied: false,
      });
      continue;
    }

    const ref: ArtifactRef = { kind: recipe.artifact, name: recipe.target };
    const result: RecipeCycleResult = {
      recipe: recipe.name,
      kind: recipe.kind,
      target: recipe.target,
      eligible: recipe.eligible(eligibleRecords, feedback),
      applied: false,
    };

    if (!result.eligible) {
      result.skipReason = "not enough signal for this recipe";
      report.results.push(result);
      continue;
    }

    try {
      const current = (await deps.artifact.read(ref)) ?? "";
      const candidate = await recipe.propose({
        records: eligibleRecords,
        feedback,
        current,
        llm: deps.llm,
      });
      result.candidate = candidate;

      const evaluation = await evaluateCandidate({
        current,
        candidate,
        tasks: tasksFromRecords(eligibleRecords, feedback),
        runner: deps.runner,
        minImprovement: deps.minImprovement,
      });
      result.evaluation = evaluation;

      await markTrained(deps.store, eligibleRecords.map((record) => record.recordId), now);

      if (evaluation.winner === "candidate" && deps.config.autoApply) {
        const published = await deps.surface.publish(
          ref,
          candidate.content,
          `reef: ${recipe.name} v-next (${candidate.target})`,
        );
        result.version = published.version;
        result.commit = published.commit;
        result.applied = published.version > 0;
        if (recipe.kind === "weight") {
          const modelId = candidate.meta?.modelId;
          const agentId = candidate.meta?.agentId;
          if (typeof modelId === "string" || typeof agentId === "string") {
            await deps.surface.reloadWeight({
              modelId: typeof modelId === "string" ? modelId : undefined,
              agentId: typeof agentId === "string" ? agentId : undefined,
            });
          }
        }
      } else if (evaluation.winner === "candidate") {
        result.skipReason = "candidate won but autoApply is off";
      } else {
        result.skipReason = "current version won the evaluation";
      }
    } catch (err) {
      result.error = errorText(err);
      result.applied = false;
      logger(`recipe ${recipe.name} failed: ${result.error}`);
    }

    report.results.push(result);
  }

  return report;
}

async function markTrained(store: ReefStore, recordIds: string[], now: () => Date): Promise<void> {
  const stamp = now().toISOString();
  await Promise.all(
    recordIds.map((recordId) => store.records.update(recordId, { trainedAt: stamp })),
  );
}
