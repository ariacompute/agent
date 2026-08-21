import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { Engine } from "@ariacompute/engine-ts";
import type { EngineStreamEvent } from "./engine.ts";
import { parseEngineResult } from "./engine.ts";

/** Minimal engine surface used by the adapter (matches @ariacompute/engine-ts Engine). */
export interface EngineLike {
  complete(
    messages: { role: string; content: string }[],
    options?: Record<string, unknown>,
    tools?: unknown[],
  ): unknown;
  close?(): void;
}

export type EngineFactory = (bundlePath: string, ffiLib?: string) => EngineLike;

/** Real factory backed by @ariacompute/engine-ts. */
export function createEngineFactory(sdk: {
  Engine: new (bundlePath: string) => EngineLike;
}): EngineFactory {
  return (bundlePath: string, ffiLib?: string) => {
    if (ffiLib) {
      process.env.ARIA_FFI_LIB = ffiLib;
    }
    return new sdk.Engine(bundlePath);
  };
}

/** Default factory using the published SDK. */
export const defaultEngineFactory: EngineFactory = createEngineFactory({ Engine });

/**
 * Run one completion via the (in-process) engine and normalize its JSON result
 * into engine stream events. Any native failure is mapped to a typed AriaError.
 */
export function generate(
  factory: EngineFactory,
  bundlePath: string,
  ffiLib: string | undefined,
  messages: unknown[],
  options: Record<string, unknown>,
  tools: unknown[] | undefined,
): EngineStreamEvent[] {
  let engine: EngineLike;
  try {
    engine = factory(bundlePath, ffiLib);
  } catch (err) {
    throw new AriaError(
      `failed to load model bundle at ${bundlePath}${ffiLib ? ` (ARIA_FFI_LIB=${ffiLib})` : ""}: ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.ENGINE_UNREACHABLE,
    );
  }
  try {
    const result = engine.complete(
      messages as { role: string; content: string }[],
      options,
      tools,
    );
    return parseEngineResult(result);
  } catch (err) {
    throw new AriaError(
      `engine.complete failed: ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.ENGINE,
    );
  } finally {
    engine.close?.();
  }
}
