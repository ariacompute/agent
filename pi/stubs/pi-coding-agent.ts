export interface ExtensionAPI {
  registerProvider(...args: unknown[]): void;
  registerTool(def: unknown): void;
  registerCommand(name: string, options: unknown): void;
  on(event: string, handler: (...args: unknown[]) => unknown): void;
}
