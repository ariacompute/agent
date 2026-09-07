import { createHarnessRecipe, type HarnessRecipeOptions, type Recipe } from "./recipe.ts";
import type { LlmProposer } from "./llm.ts";

export const DEFAULT_RULES_TARGET = "rules/guardrails";

export interface RulesRecipeOptions {
  name?: string;
  target?: string;
  minSamples?: number;
  scoreThreshold?: number;
  llm?: LlmProposer;
}

/** Evolve agent rules and guardrails. */
export function createRulesRecipe(options: RulesRecipeOptions = {}): Recipe {
  const config: HarnessRecipeOptions = {
    name: options.name ?? "rules",
    kind: "rules",
    target: options.target ?? DEFAULT_RULES_TARGET,
    minSamples: options.minSamples,
    scoreThreshold: options.scoreThreshold,
    llm: options.llm,
  };
  return createHarnessRecipe(config);
}
