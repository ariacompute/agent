import type { Context } from "@deepseek-ai/cordis";
import { loadConfig } from "../../shared/src/index.ts";
import { AriaAdapter } from "./adapter.ts";

export const name = "aria-engine";
export const inject = ["llm"];

export interface Config {
  baseUrl?: string;
  model?: string;
}

export function apply(ctx: Context, config: Config = {}): void {
  const bridge = loadConfig();
  const engineUrl = config.baseUrl ? bridgeUrl(config.baseUrl) : bridge.engineUrl;
  ctx.llm.registerAdapter(["aria"], new AriaAdapter({ engineUrl, defaultModel: config.model }));
}

function bridgeUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  return trimmed.endsWith("/v1") ? trimmed : `${trimmed}/v1`;
}

export { AriaAdapter, toDshChunks } from "./adapter.ts";
