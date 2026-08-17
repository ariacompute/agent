export function createProvider(opts: unknown): unknown {
  return opts;
}

export function openAICompletionsApi(): { id: string } {
  return { id: "openai-completions" };
}
