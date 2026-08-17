export interface Context {
  llm: {
    registerAdapter(providers: string[], adapter: unknown): unknown;
  };
  tools: {
    register(def: unknown): unknown;
  };
  on(event: string, listener: (...args: unknown[]) => unknown): unknown;
}
