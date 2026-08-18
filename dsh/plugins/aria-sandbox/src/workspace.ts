import { mkdir, readFile, readdir, stat, writeFile } from "node:fs/promises";
import { join, posix, relative, sep } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { SandboxClient, WorkspaceRegistryLike } from "./types.ts";

export const DEFAULT_WORKSPACE_ID = "default";
export const SANDBOX_WORKSPACE_DIR = "/workspace";

/**
 * Workspace ids only allow safe chars; anything else collapses to '-'.
 * `.`/`..`/empty always fall back to the default id so ids can never
 * traverse directories.
 */
export function normalizeWorkspaceId(raw: string | undefined): string {
  const cleaned = (raw ?? "")
    .replace(/[^A-Za-z0-9._-]/g, "-")
    .replace(/-+/g, "-")
    .replace(/^-+|-+$/g, "");
  if (cleaned.length === 0 || cleaned === "." || cleaned === "..") {
    return DEFAULT_WORKSPACE_ID;
  }
  return cleaned;
}

export function workspaceHostDir(root: string, workspaceId: string): string {
  return join(root, normalizeWorkspaceId(workspaceId));
}

/**
 * Create the host dir for a workspace (0700) so distinct agents never see
 * each other's files. Idempotent.
 */
export async function ensureHostWorkspace(root: string, workspaceId: string): Promise<string> {
  const dir = workspaceHostDir(root, workspaceId);
  await mkdir(dir, { recursive: true, mode: 0o700 });
  return dir;
}

/**
 * Register the workspace with dsh ctx.workspaceRegistry (persisted by dsh).
 * Falls back silently to a plain host dir when the registry is unavailable
 * or already knows the path — the plugin must never break the agent loop.
 */
export async function registerWorkspace(
  registry: WorkspaceRegistryLike | undefined,
  hostDir: string,
  workspaceId: string,
): Promise<void> {
  if (!registry) {
    return;
  }
  try {
    await registry.create(hostDir, workspaceId);
  } catch {
    // Already registered or registry not fully wired: harmless.
  }
}

/**
 * Reject sandbox-side paths that escape the workspace: only absolute paths
 * under `base` (default /workspace) with no ".." segments are allowed.
 */
export function assertSandboxPath(input: string, base: string = SANDBOX_WORKSPACE_DIR): string {
  if (typeof input !== "string" || input.trim().length === 0) {
    throw new AriaError("sandbox path must be a non-empty string", ErrorCode.INVALID_PARAM);
  }
  if (!posix.isAbsolute(input)) {
    throw new AriaError(`sandbox path must be absolute: ${input}`, ErrorCode.INVALID_PARAM);
  }
  const normBase = posix.normalize(base).replace(/\/+$/, "") || "/";
  const norm = posix.normalize(input);
  const segments = norm.split("/").filter(Boolean);
  if (segments.includes("..")) {
    throw new AriaError(`sandbox path escapes workspace: ${input}`, ErrorCode.INVALID_PARAM);
  }
  if (normBase === "/") {
    return norm;
  }
  if (norm !== normBase && !norm.startsWith(`${normBase}/`)) {
    throw new AriaError(
      `sandbox path must be under ${normBase}: ${input}`,
      ErrorCode.INVALID_PARAM,
    );
  }
  return norm;
}

export interface HostFileEntry {
  rel: string;
  abs: string;
}

/** Recursively list host workspace files as workspace-relative paths. */
export async function collectHostFiles(hostDir: string): Promise<HostFileEntry[]> {
  const out: HostFileEntry[] = [];
  async function walk(dir: string): Promise<void> {
    const entries = await readdir(dir, { withFileTypes: true });
    for (const entry of entries) {
      const abs = join(dir, entry.name);
      const rel = relative(hostDir, abs).split(sep).join("/");
      if (entry.isDirectory()) {
        await walk(abs);
      } else if (entry.isFile()) {
        out.push({ rel, abs });
      }
    }
  }
  await walk(hostDir);
  return out;
}

/**
 * Push host workspace files into the sandbox /workspace dir (restores state
 * after the MicroVM was recreated). Directories are created on demand.
 */
export async function syncFromHost(
  sandbox: SandboxClient,
  hostDir: string,
  base: string = SANDBOX_WORKSPACE_DIR,
): Promise<number> {
  const files = await collectHostFiles(hostDir);
  for (const file of files) {
    const remote = posix.join(base, file.rel);
    const parent = posix.dirname(remote);
    try {
      await sandbox.files.makeDir(parent);
    } catch {
      // already exists
    }
    const data = await readFile(file.abs);
    await sandbox.files.write(remote, data);
  }
  return files.length;
}

/** Recursively list sandbox files under a remote dir. */
export async function listSandboxFiles(
  sandbox: SandboxClient,
  base: string = SANDBOX_WORKSPACE_DIR,
): Promise<Array<{ path: string }>> {
  const out: Array<{ path: string }> = [];
  async function walk(dir: string): Promise<void> {
    const entries = await sandbox.files.list(dir);
    for (const entry of entries) {
      if (entry.isDir) {
        await walk(entry.path);
      } else {
        out.push({ path: entry.path });
      }
    }
  }
  try {
    await walk(base);
  } catch {
    // workspace dir not yet created inside the sandbox
  }
  return out;
}

/**
 * Pull sandbox /workspace files back to the host dir (persists work across
 * sandbox recreation). Returns the number of files synced.
 */
export async function syncToHost(
  sandbox: SandboxClient,
  hostDir: string,
  base: string = SANDBOX_WORKSPACE_DIR,
): Promise<number> {
  const files = await listSandboxFiles(sandbox, base);
  for (const file of files) {
    const rel = relative(base, file.path).split(sep).join("/");
    if (rel.startsWith("..") || posix.isAbsolute(rel)) {
      throw new AriaError(`sandbox file escapes workspace: ${file.path}`, ErrorCode.WORKSPACE);
    }
    const abs = join(hostDir, rel);
    await mkdir(join(hostDir, posix.dirname(rel)), { recursive: true });
    const data = await sandbox.files.read(file.path);
    await writeFile(abs, data);
  }
  return files.length;
}

export interface WorkspaceStatus {
  workspaceId: string;
  hostDir: string;
  files: number;
  totalBytes: number;
}

export async function workspaceStatus(
  hostDir: string,
  workspaceId: string,
): Promise<WorkspaceStatus> {
  const files = await collectHostFiles(hostDir);
  let totalBytes = 0;
  for (const file of files) {
    const info = await stat(file.abs);
    totalBytes += info.size;
  }
  return { workspaceId, hostDir, files: files.length, totalBytes };
}
