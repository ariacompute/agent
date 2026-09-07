import { describe, it, expect } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import { createFakeProposer } from "../src/recipes/llm.ts";
import { createPromptRecipe } from "../src/recipes/prompt.ts";
import {
  createHarnessRecipe,
  createRecipeRegistry,
  harnessEligible,
} from "../src/recipes/recipe.ts";
import { createRulesRecipe } from "../src/recipes/rules.ts";
import { createSkillRecipe } from "../src/recipes/skillclaw.ts";
import { makeFeedback, makeRecord } from "./helpers.ts";

const records = [makeRecord({ recordId: "r1" })];
const lowFeedback = [makeFeedback({ recordId: "r1", score: 0.2, text: "missing citations" })];
const highFeedback = [makeFeedback({ recordId: "r1", score: 0.95 })];

describe("harnessEligible", () => {
  it("needs samples, a score and room to improve", () => {
    expect(harnessEligible(records, lowFeedback)).toBe(true);
    expect(harnessEligible(records, highFeedback)).toBe(false);
    expect(harnessEligible([], lowFeedback)).toBe(false);
    expect(harnessEligible(records, [makeFeedback({ recordId: "r1", text: "no score" })])).toBe(false);
    expect(harnessEligible(records, lowFeedback, { minSamples: 3 })).toBe(false);
    expect(harnessEligible(records, lowFeedback, { scoreThreshold: 0.1 })).toBe(false);
  });
});

describe("skill recipe", () => {
  it("proposes a full new artifact with a diff", async () => {
    const recipe = createSkillRecipe({ llm: createFakeProposer("new skill with citations") });
    expect(recipe.name).toBe("skillclaw");
    expect(recipe.kind).toBe("harness");
    expect(recipe.artifact).toBe("skill");
    expect(recipe.target).toBe("skills/agent");
    expect(recipe.eligible(records, lowFeedback)).toBe(true);

    const result = await recipe.propose({
      records,
      feedback: lowFeedback,
      current: "old skill",
      llm: createFakeProposer("new skill with citations"),
    });
    expect(result.content).toBe("new skill with citations");
    expect(result.diff).toContain("--- a/skill/skills/agent");
    expect(result.diff).toContain("+new skill with citations");
    expect(result.score).toBeCloseTo(0.2);
    expect(result.recipeKind).toBe("harness");
  });

  it("fails loudly without a proposer or empty proposals", async () => {
    const recipe = createSkillRecipe();
    await expect(recipe.propose({ records, feedback: lowFeedback, current: "" })).rejects.toThrow(
      /no LLM proposer/,
    );

    const empty = createSkillRecipe({ llm: createFakeProposer("   ") });
    await expect(empty.propose({ records, feedback: lowFeedback, current: "" })).rejects.toThrow(
      /empty proposal/,
    );
    try {
      await empty.propose({ records, feedback: lowFeedback, current: "" });
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.REEF);
    }
  });

  it("passes feedback context into the prompt", async () => {
    let seen = "";
    const recipe = createSkillRecipe({
      llm: createFakeProposer((prompt) => {
        seen = prompt;
        return "x";
      }),
    });
    await recipe.propose({ records, feedback: lowFeedback, current: "old" });
    expect(seen).toContain("missing citations");
    expect(seen).toContain("old");
  });
});

describe("prompt and rules recipes", () => {
  it("target their own artifacts", async () => {
    const prompt = createPromptRecipe({ llm: createFakeProposer("new prompt") });
    const rules = createRulesRecipe({ llm: createFakeProposer("new rules") });
    expect(prompt.target).toBe("prompts/system");
    expect(rules.target).toBe("rules/guardrails");
    expect(prompt.artifact).toBe("prompt");
    expect(rules.artifact).toBe("rules");
    const candidate = await prompt.propose({ records, feedback: lowFeedback, current: "" });
    expect(candidate.content).toBe("new prompt");
  });

  it("honours custom targets and thresholds", () => {
    const recipe = createHarnessRecipe({
      name: "custom",
      kind: "rules",
      target: "rules/team",
      scoreThreshold: 0.99,
    });
    expect(recipe.target).toBe("rules/team");
    expect(recipe.eligible(records, lowFeedback)).toBe(true);
    // scoreThreshold override: 0.95 is "good enough" below 0.99, unlike the 0.7 default.
    expect(recipe.eligible(records, highFeedback)).toBe(true);
  });
});

describe("recipe registry", () => {
  it("registers, replaces, lists and selects", () => {
    const skill = createSkillRecipe({ llm: createFakeProposer("x") });
    const prompt = createPromptRecipe({ llm: createFakeProposer("x") });
    const registry = createRecipeRegistry([skill]);
    registry.register(prompt);
    expect(registry.list().map((r) => r.name)).toEqual(["skillclaw", "prompt"]);
    expect(registry.get("prompt")?.target).toBe("prompts/system");

    const replacement = createSkillRecipe({ target: "skills/other", llm: createFakeProposer("x") });
    registry.register(replacement);
    expect(registry.list().length).toBe(2);
    expect(registry.get("skillclaw")?.target).toBe("skills/other");

    expect(registry.select(records, lowFeedback).map((r) => r.name)).toEqual(["skillclaw", "prompt"]);
    expect(registry.select(records, highFeedback)).toEqual([]);
  });
});
