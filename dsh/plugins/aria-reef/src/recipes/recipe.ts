import { AriaError, ErrorCode } from "../../../shared/src/error.ts";
import { unifiedDiff } from "../diff.ts";
import { meanScore, summarizeFeedback } from "../feedback.ts";
import type { AgentRecord, ArtifactKind, Candidate, Feedback, RecipeKind } from "../types.ts";
import { proposeArtifactEdit, type LlmProposer } from "./llm.ts";

export interface RecipeInput {
  records: AgentRecord[];
  feedback: Feedback[];
  /** Current active content of the target artifact. */
  current: string;
  /** Injected proposer (engine FFI or fake). */
  llm?: LlmProposer;
}

export interface Recipe {
  name: string;
  kind: RecipeKind;
  artifact: ArtifactKind;
  /** Artifact path inside the artifact repo, e.g. `skill/search`. */
  target: string;
  eligible(records: AgentRecord[], feedback: Feedback[]): boolean;
  propose(input: RecipeInput): Promise<Candidate>;
}

export interface RecipeRegistry {
  register(recipe: Recipe): void;
  get(name: string): Recipe | undefined;
  list(): Recipe[];
  /** Recipes that consider the given batch actionable. */
  select(records: AgentRecord[], feedback: Feedback[]): Recipe[];
}

export function createRecipeRegistry(recipes: Recipe[] = []): RecipeRegistry {
  const items: Recipe[] = [...recipes];
  return {
    register(recipe) {
      const existing = items.findIndex((item) => item.name === recipe.name);
      if (existing >= 0) {
        items[existing] = recipe;
        return;
      }
      items.push(recipe);
    },
    get(name) {
      return items.find((item) => item.name === name);
    },
    list() {
      return [...items];
    },
    select(records, feedback) {
      return items.filter((recipe) => recipe.eligible(records, feedback));
    },
  };
}

export interface HarnessRecipeOptions {
  name: string;
  kind: ArtifactKind;
  target: string;
  /** Minimum number of scored records before the recipe fires. */
  minSamples?: number;
  /** Only evolve when the mean score is below this threshold. */
  scoreThreshold?: number;
  /** Proposer used when the caller does not inject one. */
  llm?: LlmProposer;
}

/** Shared eligibility rule for harness artifacts: enough signal and room to improve. */
export function harnessEligible(
  records: AgentRecord[],
  feedback: Feedback[],
  options: { minSamples?: number; scoreThreshold?: number } = {},
): boolean {
  const minSamples = options.minSamples ?? 1;
  const threshold = options.scoreThreshold ?? 0.7;
  if (records.length < minSamples) {
    return false;
  }
  const mean = meanScore(feedback);
  if (mean === undefined) {
    return false;
  }
  return mean < threshold;
}

/**
 * Harness recipe: ask the LLM for a revised version of a skill / prompt / rules
 * artifact and emit it as a candidate with a reviewable diff.
 */
export function createHarnessRecipe(options: HarnessRecipeOptions): Recipe {
  const minSamples = options.minSamples ?? 1;
  const threshold = options.scoreThreshold ?? 0.7;

  return {
    name: options.name,
    kind: "harness",
    artifact: options.kind,
    target: options.target,
    eligible(records, feedback) {
      return harnessEligible(records, feedback, { minSamples, scoreThreshold: threshold });
    },
    async propose(input) {
      const llm = input.llm ?? options.llm;
      if (!llm) {
        throw new AriaError(
          `recipe ${options.name}: no LLM proposer configured (set ARIA_REEF_BUNDLE or inject deps.llm)`,
          ErrorCode.REEF,
        );
      }
      const summary = summarizeFeedback(input.records, input.feedback);
      const content = await proposeArtifactEdit(llm, {
        kind: options.kind,
        target: options.target,
        current: input.current,
        feedbackSummary: summary,
      });
      if (content.trim().length === 0) {
        throw new AriaError(
          `recipe ${options.name}: empty proposal for ${options.kind}/${options.target}`,
          ErrorCode.REEF,
        );
      }
      const mean = meanScore(input.feedback);
      const candidate: Candidate = {
        recipe: options.name,
        recipeKind: "harness",
        kind: options.kind,
        target: options.target,
        content,
        diff: unifiedDiff(input.current, content, `${options.kind}/${options.target}`),
        notes: `${input.records.length} record(s), ${input.feedback.length} feedback item(s)`,
      };
      if (mean !== undefined) {
        candidate.score = mean;
      }
      return candidate;
    },
  };
}
