import type { Context } from "@deepseek-ai/cordis";
import { loadConfig } from "../../shared/src/index.ts";
import { AriaAdapter } from "./adapter.ts";

export const name = "aria-engine";
export const inject = ["llm"];

export interface Config {
  /** Local engine bundle path (overrides ARIA_ENGINE_BUNDLE). */
  bundle?: string;
  /** Native FFI lib path (overrides ARIA_FFI_LIB). */
  ffiLib?: string;
  /** Default model id passed to the engine. */
  model?: string;
}

export function apply(ctx: Context, config: Config = {}): void {
  const bridge = loadConfig();
  const bundle = config.bundle ?? bridge.engineBundle;
  const ffiLib = config.ffiLib ?? bridge.engineFfiLib;

  if (!bundle) {
    throw new Error(
      "aria-engine: no bundle configured. Set ARIA_ENGINE_BUNDLE (or cordis config.bundle). " +
        "Optional HTTP fallback via ARIA_ENGINE_URL is not supported by the in-process adapter.",
    );
  }

  ctx.llm.registerAdapter(
    ["aria"],
    new AriaAdapter({
      bundlePath: bundle,
      ffiLib,
      defaultModel: config.model ?? bridge.defaultModel,
    }),
  );
}

export { AriaAdapter, toDshChunks } from "./adapter.ts";
