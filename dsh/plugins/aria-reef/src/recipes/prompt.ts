import { createHarnessRecipe, type HarnessRecipeOptions, type Recipe } from "./recipe.ts";
import type { LlmProposer } from "./llm.ts";

export const DEFAULT_PROMPT_TARGET = "prompts/system";

export interface PromptRecipeOptions {
  name?: string;
  target?: string;
  minSamples?: number;
  scoreThreshold?: number;
  llm?: LlmProposer;
}

/** Evolve the agent system prompt / standing instructions. */
export function createPromptRecipe(options: PromptRecipeOptions = {}): Recipe {
  const config: HarnessRecipeOptions = {
    name: options.name ?? "prompt",
    kind: "prompt",
    target: options.target ?? DEFAULT_PROMPT_TARGET,
    minSamples: options.minSamples,
    scoreThreshold: options.scoreThreshold,
    llm: options.llm,
  };
  return createHarnessRecipe(config);
}
