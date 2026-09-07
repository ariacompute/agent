import { createHarnessRecipe, type HarnessRecipeOptions, type Recipe } from "./recipe.ts";
import type { LlmProposer } from "./llm.ts";

export const DEFAULT_SKILL_TARGET = "skills/agent";

export interface SkillRecipeOptions {
  name?: string;
  target?: string;
  minSamples?: number;
  scoreThreshold?: number;
  llm?: LlmProposer;
}

/**
 * SkillClaw-style skill evolution: rewrite the agent skill file from feedback.
 */
export function createSkillRecipe(options: SkillRecipeOptions = {}): Recipe {
  const config: HarnessRecipeOptions = {
    name: options.name ?? "skillclaw",
    kind: "skill",
    target: options.target ?? DEFAULT_SKILL_TARGET,
    minSamples: options.minSamples,
    scoreThreshold: options.scoreThreshold,
    llm: options.llm,
  };
  return createHarnessRecipe(config);
}
