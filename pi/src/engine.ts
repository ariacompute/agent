import {
  listModels,
  loadConfig,
  normalizeEngineUrl,
  type ModelInfo,
} from "../../packages/aria-bridge/src/index.ts";

export interface PiLike {
  registerProvider(provider: unknown): void;
}

export interface ProviderDeps {
  createProvider: (opts: Record<string, unknown>) => unknown;
  openAICompletionsApi: () => unknown;
  listModelsFn?: typeof listModels;
  fetchImpl?: typeof fetch;
}

export async function registerAriaEngine(pi: PiLike, deps: ProviderDeps): Promise<void> {
  const cfg = loadConfig();
  const baseUrl = normalizeEngineUrl(process.env.ARIA_ENGINE_URL ?? cfg.engineUrl);
  const list = deps.listModelsFn ?? listModels;
  let models: Array<Record<string, unknown>> = [];
  let catalogError: string | undefined;
  try {
    const discovered: ModelInfo[] = await list(baseUrl, { fetchImpl: deps.fetchImpl });
    models = discovered.map((m) => ({
      id: m.id,
      name: m.id,
      reasoning: false,
      input: ["text"],
      cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
      contextWindow: 8192,
      maxTokens: 2048,
    }));
  } catch (err) {
    catalogError = err instanceof Error ? err.message : String(err);
  }

  const provider = deps.createProvider({
    id: "aria",
    name: "Aria Engine",
    baseUrl,
    auth: {
      apiKey: {
        name: "Aria Engine (local, no key)",
        async resolve() {
          return { auth: { apiKey: "" }, source: "local no-auth" };
        },
      },
    },
    models,
    api: deps.openAICompletionsApi(),
    catalogError,
  });
  pi.registerProvider(provider);
}
