/** Minimal structural types so tests can run offline with injected fakes. */

export interface SandboxCommandResult {
  stdout: string;
  stderr: string;
  exitCode: number;
}

export interface SandboxFileInfo {
  name: string;
  path: string;
  isDir: boolean;
}

export interface SandboxClient {
  kill(): Promise<void>;
  commands: {
    run(
      command: string,
      options?: { cwd?: string; env?: Record<string, string>; timeout?: number },
    ): Promise<SandboxCommandResult>;
  };
  files: {
    write(path: string, data: Uint8Array | string): Promise<void>;
    read(path: string): Promise<Uint8Array>;
    list(path: string): Promise<SandboxFileInfo[]>;
    makeDir(path: string): Promise<void>;
  };
}

export interface SandboxFactory {
  create(options: {
    apiKey?: string;
    timeoutMs?: number;
    template?: string;
  }): Promise<SandboxClient>;
}

/** Duck-type of dsh ctx.workspaceRegistry (from @deepseek-ai/dsh-workspace). */
export interface WorkspaceRegistryLike {
  get(): unknown;
  create(path: string, title?: string): unknown;
}

export interface SandboxPluginDeps {
  /** E2B sandbox factory; defaults to e2bSandboxFactory (real CubeSandbox). */
  factory?: SandboxFactory;
  /** dsh workspace registry; when missing the plugin falls back to plain host dirs. */
  registry?: WorkspaceRegistryLike;
  /** Overrides config.workspaceRoot (useful in tests). */
  workspaceRoot?: string;
}
