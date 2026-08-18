import type { Context } from "@deepseek-ai/cordis";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { loadSandboxConfig, withOverrides, type SandboxConfig } from "./config.ts";
import { e2bSandboxFactory } from "./e2b-client.ts";
import type { SandboxClient, SandboxPluginDeps } from "./types.ts";
import {
  assertSandboxPath,
  ensureHostWorkspace,
  normalizeWorkspaceId,
  registerWorkspace,
  SANDBOX_WORKSPACE_DIR,
  syncFromHost,
  syncToHost,
  workspaceStatus,
} from "./workspace.ts";

export const name = "aria-sandbox";
export const inject = ["tools"];

export type { SandboxConfig } from "./config.ts";

interface ToolExecContext {
  signal?: AbortSignal;
  sessionId?: string;
  context?: { sessionId?: string };
}

function sessionIdFrom(exec: ToolExecContext | undefined): string | undefined {
  return exec?.sessionId ?? exec?.context?.sessionId;
}

export function apply(
  ctx: Context,
  config: Partial<SandboxConfig> = {},
  deps: SandboxPluginDeps = {},
): void {
  const cfg = withOverrides(loadSandboxConfig(), config);
  const factory = deps.factory ?? e2bSandboxFactory;
  const workspaceRoot = deps.workspaceRoot ?? cfg.workspaceRoot;
  const registry = deps.registry;

  // One sandbox + one host dir per workspace id; never shared across ids.
  const sandboxes = new Map<string, Promise<SandboxClient>>();
  const hostDirs = new Map<string, string>();
  const registered = new Set<string>();

  function resolveWorkspaceId(
    explicit: string | undefined,
    exec: ToolExecContext | undefined,
  ): string {
    return normalizeWorkspaceId(explicit ?? sessionIdFrom(exec) ?? cfg.workspaceId);
  }

  async function ensureSandbox(workspaceId: string): Promise<SandboxClient> {
    let pending = sandboxes.get(workspaceId);
    if (!pending) {
      pending = factory
        .create({
          apiKey: cfg.apiKey,
          timeoutMs: cfg.timeoutMs,
          template: cfg.template,
        })
        .catch((err: unknown) => {
          sandboxes.delete(workspaceId);
          const cause = err instanceof Error ? err.message : String(err);
          throw new AriaError(
            `failed to create CubeSandbox sandbox: ${cause}`,
            ErrorCode.SANDBOX,
          );
        });
      sandboxes.set(workspaceId, pending);
    }
    return pending;
  }

  async function ensureWorkspace(workspaceId: string): Promise<string> {
    let hostDir = hostDirs.get(workspaceId);
    if (!hostDir) {
      hostDir = await ensureHostWorkspace(workspaceRoot, workspaceId);
      hostDirs.set(workspaceId, hostDir);
      if (!registered.has(workspaceId)) {
        await registerWorkspace(registry, hostDir, workspaceId);
        registered.add(workspaceId);
      }
    }
    return hostDir;
  }

  async function withSandbox<T>(
    workspaceId: string,
    fn: (sandbox: SandboxClient) => Promise<T>,
  ): Promise<T> {
    const sandbox = await ensureSandbox(workspaceId);
    try {
      return await fn(sandbox);
    } catch (err) {
      if (err instanceof AriaError) {
        throw err;
      }
      const cause = err instanceof Error ? err.message : String(err);
      throw new AriaError(`sandbox operation failed: ${cause}`, ErrorCode.SANDBOX);
    }
  }

  const tools = [
    {
      name: "sandbox_exec",
      description:
        "Run a shell command inside the agent's isolated CubeSandbox MicroVM. Default cwd is /workspace (the agent workspace).",
      parameters: {
        command: { type: "string", required: true, description: "Shell command" },
        cwd: { type: "string", description: "Working directory inside sandbox (must stay under /workspace)" },
        timeout_ms: { type: "number", description: "Command timeout in ms" },
        workspace: { type: "string", description: "Optional explicit workspace id (defaults to session)" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { stdout: string; stderr: string; exitCode: number }) => [
          { type: "text", text: JSON.stringify(value) },
        ],
      },
      async execute(
        args: { command: string; cwd?: string; timeout_ms?: number; workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ stdout: string; stderr: string; exitCode: number }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const hostDir = await ensureWorkspace(workspaceId);
        const cwd = args.cwd ? assertSandboxPath(args.cwd) : SANDBOX_WORKSPACE_DIR;
        const result = await withSandbox(workspaceId, (sandbox) =>
          sandbox.commands.run(args.command, {
            cwd,
            timeout: args.timeout_ms,
          }),
        );
        if (cfg.syncAfterExec) {
          await syncToHost(await ensureSandbox(workspaceId), hostDir);
        }
        return result;
      },
    },
    {
      name: "sandbox_read_file",
      description: "Read a file from the sandbox /workspace.",
      parameters: {
        path: { type: "string", required: true, description: "Absolute path under /workspace" },
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { content: string }) => [
          { type: "text", text: value.content },
        ],
      },
      async execute(
        args: { path: string; workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ content: string }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const path = assertSandboxPath(args.path);
        const data = await withSandbox(workspaceId, (sandbox) => sandbox.files.read(path));
        return { content: Buffer.from(data).toString("utf8") };
      },
    },
    {
      name: "sandbox_write_file",
      description: "Write a file into the sandbox /workspace.",
      parameters: {
        path: { type: "string", required: true, description: "Absolute path under /workspace" },
        content: { type: "string", required: true, description: "File content" },
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { ok: boolean }) => [
          { type: "text", text: value.ok ? "written" : "failed" },
        ],
      },
      async execute(
        args: { path: string; content: string; workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ ok: boolean }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const path = assertSandboxPath(args.path);
        await withSandbox(workspaceId, (sandbox) => sandbox.files.write(path, args.content));
        return { ok: true };
      },
    },
    {
      name: "sandbox_list_files",
      description: "List files/dirs inside the sandbox /workspace.",
      parameters: {
        path: { type: "string", description: "Absolute path under /workspace (default /workspace)" },
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { entries: unknown }) => [
          { type: "text", text: JSON.stringify(value.entries) },
        ],
      },
      async execute(
        args: { path?: string; workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ entries: Array<{ name: string; path: string; isDir: boolean }> }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const path = args.path ? assertSandboxPath(args.path) : SANDBOX_WORKSPACE_DIR;
        const entries = await withSandbox(workspaceId, (sandbox) => sandbox.files.list(path));
        return { entries };
      },
    },
    {
      name: "sandbox_sync_to_host",
      description:
        "Pull all sandbox /workspace files into the persistent host workspace dir (survives sandbox recreation).",
      parameters: {
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { synced: number }) => [
          { type: "text", text: `synced ${value.synced} files` },
        ],
      },
      async execute(
        args: { workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ synced: number }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const hostDir = await ensureWorkspace(workspaceId);
        const synced = await withSandbox(workspaceId, (sandbox) =>
          syncToHost(sandbox, hostDir),
        );
        return { synced };
      },
    },
    {
      name: "sandbox_sync_from_host",
      description:
        "Push persistent host workspace files back into the sandbox /workspace (restores state after sandbox recreation).",
      parameters: {
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: { synced: number }) => [
          { type: "text", text: `synced ${value.synced} files` },
        ],
      },
      async execute(
        args: { workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ synced: number }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const hostDir = await ensureWorkspace(workspaceId);
        const synced = await withSandbox(workspaceId, (sandbox) =>
          syncFromHost(sandbox, hostDir),
        );
        return { synced };
      },
    },
    {
      name: "workspace_status",
      description: "Show the persistent host workspace dir, file count and total bytes.",
      parameters: {
        workspace: { type: "string", description: "Optional explicit workspace id" },
      },
      output: {
        schema: { type: "object" },
        render: (_args: unknown, value: unknown) => [{ type: "text", text: JSON.stringify(value) }],
      },
      async execute(
        args: { workspace?: string },
        exec: ToolExecContext,
      ): Promise<{ workspaceId: string; hostDir: string; files: number; totalBytes: number }> {
        const workspaceId = resolveWorkspaceId(args.workspace, exec);
        const hostDir = await ensureWorkspace(workspaceId);
        return workspaceStatus(hostDir, workspaceId);
      },
    },
  ];

  for (const tool of tools) {
    ctx.tools.register(tool);
  }

  ctx.on("dispose", async () => {
    const clients = await Promise.allSettled([...sandboxes.values()]);
    for (const result of clients) {
      if (result.status === "fulfilled") {
        try {
          await result.value.kill();
        } catch {
          // already gone
        }
      }
    }
    sandboxes.clear();
  });
}

export { e2bSandboxFactory } from "./e2b-client.ts";
export {
  assertSandboxPath,
  normalizeWorkspaceId,
  syncFromHost,
  syncToHost,
} from "./workspace.ts";
export type { SandboxClient, SandboxFactory, SandboxPluginDeps } from "./types.ts";
