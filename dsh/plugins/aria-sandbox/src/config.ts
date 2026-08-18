import { homedir } from "node:os";
import { join } from "node:path";

export const DEFAULT_E2B_API_URL = "http://127.0.0.1:3000";
export const DEFAULT_E2B_API_KEY = "e2b_000000";
export const DEFAULT_TIMEOUT_MS = 300_000;

export interface SandboxConfig {
  /** E2B API key sent to CubeAPI (local deployments accept any placeholder). */
  apiKey: string;
  /** CubeAPI base URL (E2B-compatible). */
  apiUrl: string;
  /** CubeSandbox template id (CUBE_TEMPLATE_ID). Optional. */
  template?: string;
  /** Sandbox idle timeout in ms before CubeAPI kills it. */
  timeoutMs: number;
  /** Host-side root dir that holds one sub-dir per agent workspace. */
  workspaceRoot: string;
  /** Sync sandbox /workspace back to host after every sandbox_exec. */
  syncAfterExec: boolean;
  /** Default workspace id when neither tool args nor session id provide one. */
  workspaceId?: string;
}

export function defaultWorkspaceRoot(): string {
  return join(homedir(), ".ariacompute", "agent", "workspaces");
}

function parseBool(raw: string | undefined): boolean {
  if (raw === undefined) {
    return false;
  }
  return /^(1|true|yes|on)$/i.test(raw.trim());
}

function parseTimeoutMs(raw: string | undefined): number {
  const value = Number(raw);
  return Number.isFinite(value) && value > 0 ? Math.floor(value) : DEFAULT_TIMEOUT_MS;
}

export function loadSandboxConfig(
  env: Record<string, string | undefined> = process.env,
): SandboxConfig {
  return {
    apiKey: env.E2B_API_KEY?.trim() || DEFAULT_E2B_API_KEY,
    apiUrl: env.E2B_API_URL?.trim() || DEFAULT_E2B_API_URL,
    template: env.CUBE_TEMPLATE_ID?.trim() || undefined,
    timeoutMs: parseTimeoutMs(env.E2B_TIMEOUT_MS),
    workspaceRoot: env.ARIA_WORKSPACE_ROOT?.trim() || defaultWorkspaceRoot(),
    syncAfterExec: parseBool(env.ARIA_WORKSPACE_SYNC_AFTER_EXEC),
    workspaceId: env.ARIA_WORKSPACE_ID?.trim() || undefined,
  };
}

export function withOverrides(base: SandboxConfig, overrides: Partial<SandboxConfig>): SandboxConfig {
  return {
    apiKey: overrides.apiKey ?? base.apiKey,
    apiUrl: overrides.apiUrl ?? base.apiUrl,
    template: overrides.template ?? base.template,
    timeoutMs: overrides.timeoutMs ?? base.timeoutMs,
    workspaceRoot: overrides.workspaceRoot ?? base.workspaceRoot,
    syncAfterExec: overrides.syncAfterExec ?? base.syncAfterExec,
    workspaceId: overrides.workspaceId ?? base.workspaceId,
  };
}
