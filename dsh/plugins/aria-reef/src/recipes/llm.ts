import { AriaError, ErrorCode } from "../../../shared/src/error.ts";
import type { ArtifactKind } from "../types.ts";

/**
 * Minimal engine surface used to propose artifact edits. Structurally identical
 * to the `EngineLike` in `aria-engine`; redeclared here so that plugins stay
 * independent (aria-reef depends on `shared` only).
 */
export interface EngineLike {
  complete(
    messages: { role: string; content: string }[],
    options?: Record<string, unknown>,
    tools?: unknown[],
  ): unknown;
  close?(): void;
}

export type EngineFactory = (bundlePath: string, ffiLib?: string) => EngineLike;

/** Anything that can turn a prompt into a proposal. */
export interface LlmProposer {
  propose(prompt: string): Promise<string>;
}

export interface EngineProposerOptions {
  bundlePath: string;
  ffiLib?: string;
  model?: string;
  maxTokens?: number;
  temperature?: number;
}

/** Best-effort extraction of the assistant text out of an engine result. */
export function extractCompletionText(result: unknown): string {
  if (typeof result === "string") {
    return result;
  }
  if (Array.isArray(result)) {
    const chunks = result
      .map((event) => {
        if (typeof event === "string") {
          return event;
        }
        if (event && typeof event === "object" && "text" in event) {
          const text = (event as { text?: unknown }).text;
          return typeof text === "string" ? text : "";
        }
        return "";
      })
      .filter((text): text is string => typeof text === "string");
    return chunks.join("");
  }
  if (result && typeof result === "object") {
    const value = result as Record<string, unknown>;
    if (typeof value.content === "string") {
      return value.content;
    }
    const choices = value.choices;
    if (Array.isArray(choices) && choices.length > 0) {
      const first = choices[0] as Record<string, unknown>;
      if (first && typeof first === "object") {
        if (typeof first.text === "string") {
          return first.text;
        }
        const message = first.message as { content?: unknown } | undefined;
        if (message && typeof message.content === "string") {
          return message.content;
        }
      }
    }
  }
  return "";
}

/** Strip markdown fences an LLM may wrap around the artifact content. */
export function cleanProposed(raw: string): string {
  const trimmed = raw.trim();
  const fenced = /^```[a-zA-Z0-9_-]*\n([\s\S]*?)\n```$/.exec(trimmed);
  return (fenced ? fenced[1] : trimmed).trim();
}

/** Propose via in-process engine FFI (`@ariacompute/engine-ts`). */
export function createEngineProposer(
  factory: EngineFactory,
  options: EngineProposerOptions,
): LlmProposer {
  return {
    async propose(prompt) {
      let engine: EngineLike;
      try {
        engine = factory(options.bundlePath, options.ffiLib);
      } catch (err) {
        throw new AriaError(
          `reef: failed to load model bundle at ${options.bundlePath}: ${err instanceof Error ? err.message : String(err)}`,
          ErrorCode.ENGINE_UNREACHABLE,
        );
      }
      try {
        const result = engine.complete(
          [{ role: "user", content: prompt }],
          {
            ...(options.model ? { model: options.model } : {}),
            ...(options.maxTokens !== undefined ? { max_tokens: options.maxTokens } : {}),
            ...(options.temperature !== undefined ? { temperature: options.temperature } : {}),
          },
        );
        return cleanProposed(extractCompletionText(result));
      } catch (err) {
        throw new AriaError(
          `reef: engine proposal failed: ${err instanceof Error ? err.message : String(err)}`,
          ErrorCode.ENGINE,
        );
      } finally {
        engine.close?.();
      }
    },
  };
}

/**
 * Resolve the published engine SDK lazily. The specifier is dynamic so the
 * plugin loads (and typechecks) without the optional native package present;
 * a missing SDK surfaces as a typed error at call time, never silently.
 */
export async function loadDefaultEngineFactory(): Promise<EngineFactory> {
  const specifier = "@ariacompute/engine-ts";
  let mod: { Engine?: new (bundlePath: string) => EngineLike };
  try {
    mod = (await import(specifier)) as { Engine?: new (bundlePath: string) => EngineLike };
  } catch (err) {
    throw new AriaError(
      `reef: cannot load ${specifier} (install it or inject deps.llm): ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.ENGINE_UNREACHABLE,
    );
  }
  if (typeof mod?.Engine !== "function") {
    throw new AriaError(
      `reef: ${specifier} does not export Engine`,
      ErrorCode.ENGINE_UNREACHABLE,
    );
  }
  const EngineCtor = mod.Engine;
  return (bundlePath: string, ffiLib?: string) => {
    if (ffiLib) {
      process.env.ARIA_FFI_LIB = ffiLib;
    }
    return new EngineCtor(bundlePath);
  };
}

export interface ArtifactEditRequest {
  kind: ArtifactKind;
  target: string;
  current: string;
  feedbackSummary: string;
}

const ROLE: Record<ArtifactKind, string> = {
  skill: "an agent SKILL definition (markdown, with frontmatter if present)",
  prompt: "the agent system prompt / standing instructions",
  rules: "the agent rules and guardrails document",
  weight: "a model weight training specification",
};

/** Ask the LLM for the full new content of one harness artifact. */
export async function proposeArtifactEdit(
  llm: LlmProposer,
  request: ArtifactEditRequest,
): Promise<string> {
  const prompt = [
    `You are improving ${ROLE[request.kind]} named "${request.target}".`,
    "User feedback and outcomes from recent turns:",
    request.feedbackSummary.length > 0 ? request.feedbackSummary : "(no detailed feedback)",
    "",
    "Current version:",
    "----- BEGIN CURRENT -----",
    request.current.length > 0 ? request.current : "(empty)",
    "----- END CURRENT -----",
    "",
    "Return ONLY the complete new version of the artifact. No explanation, no diff, no markdown fences.",
  ].join("\n");
  const raw = await llm.propose(prompt);
  return cleanProposed(raw);
}

/** Deterministic proposer for tests and offline runs. */
export function createFakeProposer(
  responder: string | ((prompt: string) => string) = "improved",
): LlmProposer {
  return {
    async propose(prompt) {
      return typeof responder === "function" ? responder(prompt) : responder;
    },
  };
}
