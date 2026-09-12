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

/** Selectable sandbox backend. `cubesandbox` is the legacy e2b/CubeAPI path. */
export type SandboxType = "docker" | "kata" | "cubesandbox";

/**
 * Pluggable executor for the OCI CLI (docker/nerdctl). Implemented over
 * `child_process.spawn` in production and a fake in tests so the container
 * backend stays fully offline-testable.
 */
export interface ContainerExecutor {
  run(args: string[]): Promise<{ stdout: string; stderr: string; exitCode: number }>;
}

export interface SandboxFactory {
  create(options: SandboxCreateOptions): Promise<SandboxClient>;
}

/**
 * Superset of create options accepted by every backend. CubeSandbox ignores
 * the container-only fields (hostDir/image/runtime/cli); the container backend
 * ignores apiKey/template.
 */
export interface SandboxCreateOptions {
  apiKey?: string;
  timeoutMs?: number;
  template?: string;
  /** Host workspace dir mounted into the container at /workspace (docker/kata). */
  hostDir?: string;
  /** Container image (docker/kata). */
  image?: string;
  /** Container runtime; kata defaults to "kata" (docker/nerdctl --runtime). */
  runtime?: string;
  /** OCI CLI binary: "docker" | "nerdctl". */
  cli?: string;
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
