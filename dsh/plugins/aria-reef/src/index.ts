import type { Context } from "@deepseek-ai/cordis";
import {
  createAriapinClient,
  type AriapinClient,
  type FetchLike,
} from "./ariapin-client.ts";
import { createArtifactRepo, type ArtifactRepo } from "./artifact.ts";
import { loadReefConfig, withOverrides, type ReefConfig } from "./config.ts";
import { createReportTool, createStatusTool, isEligible } from "./feedback.ts";
import { createRecorder, registerServeHooks, type Recorder } from "./record.ts";
import {
  createEngineProposer,
  loadDefaultEngineFactory,
  type LlmProposer,
} from "./recipes/llm.ts";
import { createPromptRecipe } from "./recipes/prompt.ts";
import { createRecipeRegistry, type RecipeRegistry } from "./recipes/recipe.ts";
import { createRulesRecipe } from "./recipes/rules.ts";
import { createSkillRecipe } from "./recipes/skillclaw.ts";
import { createWeightRecipe } from "./recipes/weight.ts";
import { createScheduler, type Scheduler } from "./scheduler.ts";
import { createFileStore, type ReefStore } from "./store.ts";
import { createSurface, type Surface } from "./surface.ts";
import { runCycle } from "./train.ts";
import type { RunCommand } from "../../shared/src/index.ts";
import type { ArtifactRef, CycleReport } from "./types.ts";

export const name = "aria-reef";
export const inject = ["tools"];

export interface WeightConfig {
  /** ariapin base model id (must be in its allow-list). */
  baseModel?: string;
  jobType?: string;
  target?: string;
  minRollouts?: number;
  minScore?: number;
  lora?: Record<string, unknown>;
  hyperparams?: Record<string, unknown>;
  /** Agent serving the trained weights (enables the agent-restart reload fallback). */
  agentId?: string;
}

export interface ReefPluginConfig extends Partial<ReefConfig> {
  /** Weight recipe settings; `weight` is only registered when a base model is set. */
  weight?: WeightConfig;
  /** Override the artifact targets edited by each harness recipe. */
  targets?: { skill?: string; prompt?: string; rules?: string };
}

export interface ReefPluginDeps {
  store?: ReefStore;
  llm?: LlmProposer;
  runCommand?: RunCommand;
  ariapin?: AriapinClient;
  fetch?: FetchLike;
  logger?: (message: string) => void;
  /** Timer/clock injection for the auto-cycle scheduler (tests). */
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
  now?: () => number;
}

export interface ReefPlugin {
  config: ReefConfig;
  store: ReefStore;
  recorder: Recorder;
  artifact: ArtifactRepo;
  surface: Surface;
  registry: RecipeRegistry;
  /** Hot read of the active artifact version (no cache, no restart needed). */
  active(ref: ArtifactRef): Promise<{ version: number; content: string } | null>;
  /** Run one Grow -> Evaluate -> Commit -> Surface cycle. */
  cycle(): Promise<CycleReport>;
  /** Auto-cycle scheduler, only present when `autoCycle` is enabled. */
  scheduler?: Scheduler;
  tools: unknown[];
}

function defaultLogger(message: string): void {
  console.warn(`[aria-reef] ${message}`);
}

export function apply(
  ctx: Context,
  config: ReefPluginConfig = {},
  deps: ReefPluginDeps = {},
): ReefPlugin | undefined {
  const cfg = withOverrides(loadReefConfig(), config);
  if (!cfg.enabled) {
    return undefined;
  }
  const logger = deps.logger ?? defaultLogger;
  const store = deps.store ?? createFileStore(cfg.storeDir);

  const ariapin =
    deps.ariapin ??
    createAriapinClient(
      { baseUrl: cfg.ariapinUrl, apiKey: cfg.ariapinKey, timeoutMs: cfg.ariapinTimeoutMs },
      { fetch: deps.fetch },
    );

  const artifact = createArtifactRepo({ root: cfg.artifactRepo, runCommand: deps.runCommand, logger });
  const surface = createSurface({ artifact, ariapin, logger });
  const recorder = createRecorder({ store, release: cfg.release, logger });
  registerServeHooks(ctx, recorder);

  // Lazily build the engine proposer so plugin load stays synchronous.
  let pending: Promise<LlmProposer> | undefined;
  const resolveLlm = (): Promise<LlmProposer> => {
    if (deps.llm) {
      return Promise.resolve(deps.llm);
    }
    if (!cfg.bundle) {
      return Promise.reject(
        new Error("no LLM proposer configured: set ARIA_REEF_BUNDLE (or inject deps.llm)"),
      );
    }
    pending ??= loadDefaultEngineFactory().then((factory) =>
      createEngineProposer(factory, {
        bundlePath: cfg.bundle!,
        ffiLib: cfg.ffiLib,
        model: cfg.model,
      }),
    );
    return pending;
  };
  const llm: LlmProposer | undefined =
    deps.llm ??
    (cfg.bundle
      ? {
          async propose(prompt) {
            const proposer = await resolveLlm();
            return proposer.propose(prompt);
          },
        }
      : undefined);

  const registry = createRecipeRegistry([
    createSkillRecipe({ target: config.targets?.skill, llm }),
    createPromptRecipe({ target: config.targets?.prompt, llm }),
    createRulesRecipe({ target: config.targets?.rules, llm }),
  ]);

  const weightBaseModel = config.weight?.baseModel ?? cfg.baseModel;
  if (cfg.recipes.includes("weight") && weightBaseModel) {
    registry.register(
      createWeightRecipe(
        {
          baseModel: weightBaseModel,
          jobType: config.weight?.jobType,
          target: config.weight?.target,
          minRollouts: config.weight?.minRollouts,
          minScore: config.weight?.minScore,
          lora: config.weight?.lora,
          hyperparams: config.weight?.hyperparams,
          agentId: config.weight?.agentId,
        },
        { client: ariapin },
      ),
    );
  }

  const runOneCycle = (): Promise<CycleReport> =>
    runCycle({
      store,
      artifact,
      surface,
      registry,
      config: cfg,
      llm: deps.llm ?? (cfg.bundle ? llm : undefined),
      logger,
    });

  // New signals = untrained records that already pass the eligibility gate.
  const countSignals = async (): Promise<number> => {
    const records = await store.records.list({ untrainedOnly: true, limit: cfg.batchSize });
    if (records.length === 0) {
      return 0;
    }
    const feedback = await store.feedback.forRecords(records.map((record) => record.recordId));
    let count = 0;
    for (const record of records) {
      if (isEligible(record, feedback, { minFeedback: cfg.minFeedback })) {
        count += 1;
      }
    }
    return count;
  };

  // Automatic cycles are opt-in: without ARIA_REEF_CYCLE no timer is ever created.
  const scheduler = cfg.autoCycle
    ? createScheduler({
        cycle: runOneCycle,
        countSignals,
        config: {
          intervalMs: cfg.cycleIntervalMs,
          minSignals: cfg.cycleMinSignals,
          pollMs: cfg.cyclePollMs,
          backoffBaseMs: cfg.cyclePollMs,
          maxBackoffMs: cfg.cycleMaxBackoffMs,
        },
        ...(deps.setTimer ? { setTimer: deps.setTimer } : {}),
        ...(deps.clearTimer ? { clearTimer: deps.clearTimer } : {}),
        ...(deps.now ? { now: deps.now } : {}),
        logger,
      })
    : undefined;
  scheduler?.start();

  const reportTool = createReportTool({
    store,
    lastRecordId: () => recorder.lastRecordId(),
    policy: { minFeedback: cfg.minFeedback },
  });
  const statusTool = createStatusTool({
    store,
    release: cfg.release,
    recipes: cfg.recipes,
    ...(scheduler ? { scheduler: () => scheduler.status() } : {}),
  });
  const cycleTool = {
    name: "aria_reef_cycle",
    description:
      "Run one reef improvement cycle (Grow -> Evaluate -> Commit) over the recorded turns and report the outcome.",
    parameters: {},
    output: {
      schema: { type: "object" },
      render: (_args: unknown, value: unknown) => [{ type: "text", text: JSON.stringify(value) }],
    },
    async execute(): Promise<CycleReport> {
      return runOneCycle();
    },
  };

  for (const tool of [reportTool, statusTool, cycleTool]) {
    ctx.tools.register(tool);
  }

  // Artifact repo bootstrap is off the startup path.
  void artifact.init().catch((err: unknown) => {
    logger(`artifact repo init failed: ${err instanceof Error ? err.message : String(err)}`);
  });

  ctx.on("dispose", async () => {
    // Stop the timer first, then let in-flight cycles and buffered records settle.
    await scheduler?.stop();
    await recorder.flush();
  });

  return {
    config: cfg,
    store,
    recorder,
    artifact,
    surface,
    registry,
    active: (ref) => surface.active(ref),
    cycle: runOneCycle,
    ...(scheduler ? { scheduler } : {}),
    tools: [reportTool, statusTool, cycleTool],
  };
}

export { createFileStore, createMemoryStore } from "./store.ts";
export { createSurface } from "./surface.ts";
export { createArtifactRepo, assertArtifactName } from "./artifact.ts";
export { createAriapinClient, createFakeAriapinClient } from "./ariapin-client.ts";
export { createRecipeRegistry, createHarnessRecipe, harnessEligible } from "./recipes/recipe.ts";
export { createSkillRecipe } from "./recipes/skillclaw.ts";
export { createPromptRecipe } from "./recipes/prompt.ts";
export { createRulesRecipe } from "./recipes/rules.ts";
export { createWeightRecipe, collectRollouts, rolloutsToJsonl } from "./recipes/weight.ts";
export { createEngineProposer, createFakeProposer, loadDefaultEngineFactory } from "./recipes/llm.ts";
export { evaluateCandidate, tasksFromRecords, defaultRunner } from "./evaluate.ts";
export {
  createReportTool,
  createStatusTool,
  applyRubric,
  defaultRubric,
  isEligible,
  normalizeScore,
  summarizeFeedback,
} from "./feedback.ts";
export { createRecorder, registerServeHooks, promptOf, outcomeFromResponse } from "./record.ts";
export { createScheduler, SIGNAL_GATE_MS } from "./scheduler.ts";
export type {
  Scheduler,
  SchedulerConfig,
  SchedulerDeps,
  SchedulerStatus,
  TickResult,
} from "./scheduler.ts";
export { runCycle } from "./train.ts";
export { unifiedDiff } from "./diff.ts";
export { loadReefConfig, withOverrides } from "./config.ts";
export type { ReefConfig } from "./config.ts";
