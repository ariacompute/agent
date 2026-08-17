import type { ExtensionAPI } from "@earendil-works/pi-coding-agent";
import { createProvider, openAICompletionsApi } from "@earendil-works/pi-ai";
import { registerAriaEngine } from "../src/engine.ts";

export default async function ariaEngineExtension(pi: ExtensionAPI): Promise<void> {
  await registerAriaEngine(pi, { createProvider, openAICompletionsApi });
}
