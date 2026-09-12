import { homedir } from "node:os";
import { join } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { SandboxType } from "./types.ts";

export const DEFAULT_E2B_API_URL = "http://127.0.0.1:3000";
export const DEFAULT_E2B_API_KEY = "e2b_000000";
export const DEFAULT_TIMEOUT_MS = 300_000;

/** Default sandbox backend. Set ARIA_SANDBOX_TYPE to switch. */
export const DEFAULT_SANDBOX_TYPE: SandboxType = "docker";
export const DEFAULT_SANDBOX_IMAGE = "ubuntu:22.04";
export const DEFAULT_SANDBOX_CLI = "docker";
/** Runtime flag applied automatically when the backend is kata. */
export const KATA_RUNTIME = "kata";
export const SANDBOX_TYPES: SandboxType[] = ["docker", "kata", "cubesandbox"];

export interface SandboxConfig {
  /** Selected backend: docker | kata | cubesandbox. */
  sandboxType: SandboxType;
  /** E2B API key sent to CubeAPI (local deployments accept any placeholder). */
  apiKey: string;
  /** CubeAPI base URL (E2B-compatible). */
  apiUrl: string;
  /** CubeSandbox template id (CUBE_TEMPLATE_ID). Optional. */
  template?: string;
  /** Sandbox idle timeout in ms before CubeAPI kills it. */
  timeoutMs: number;
  /** Container image for docker/kata backends. */
  containerImage: string;
  /** OCI CLI binary: docker | nerdctl. */
  containerCli: string;
  /** Container runtime; kata defaults to "kata". */
  containerRuntime?: string;
  /** Host-side root dir that holds one sub-dir per agent workspace. */
  workspaceRoot: string;
  /** Sync sandbox /workspace back to host after every sandbox_exec (cubesandbox only). */
  syncAfterExec: boolean;
  /** Default workspace id when neither tool args nor session id provide one. */
  workspaceId?: string;
}

/**
 * Normalize a raw sandbox-type string into the typed enum. Unknown values fail
 * loudly (never silently fall back) so misconfiguration is caught early.
 */
export function normalizeSandboxType(raw: string | undefined): SandboxType {
  const value = raw?.trim();
  if (value === undefined || value.length === 0) {
    return DEFAULT_SANDBOX_TYPE;
  }
  if ((SANDBOX_TYPES as string[]).includes(value)) {
    return value as SandboxType;
  }
  throw new AriaError(`invalid ARIA_SANDBOX_TYPE: ${value}`, ErrorCode.INVALID_PARAM);
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
  const sandboxType = normalizeSandboxType(env.ARIA_SANDBOX_TYPE);
  const runtimeRaw = env.ARIA_SANDBOX_RUNTIME?.trim();
  const containerRuntime = runtimeRaw || (sandboxType === "kata" ? KATA_RUNTIME : undefined);
  return {
    sandboxType,
    apiKey: env.E2B_API_KEY?.trim() || DEFAULT_E2B_API_KEY,
    apiUrl: env.E2B_API_URL?.trim() || DEFAULT_E2B_API_URL,
    template: env.CUBE_TEMPLATE_ID?.trim() || undefined,
    timeoutMs: parseTimeoutMs(env.E2B_TIMEOUT_MS),
    containerImage: env.ARIA_SANDBOX_IMAGE?.trim() || DEFAULT_SANDBOX_IMAGE,
    containerCli: env.ARIA_SANDBOX_CLI?.trim() || DEFAULT_SANDBOX_CLI,
    containerRuntime,
    workspaceRoot: env.ARIA_WORKSPACE_ROOT?.trim() || defaultWorkspaceRoot(),
    syncAfterExec: parseBool(env.ARIA_WORKSPACE_SYNC_AFTER_EXEC),
    workspaceId: env.ARIA_WORKSPACE_ID?.trim() || undefined,
  };
}

export function withOverrides(base: SandboxConfig, overrides: Partial<SandboxConfig>): SandboxConfig {
  return {
    sandboxType: overrides.sandboxType ?? base.sandboxType,
    apiKey: overrides.apiKey ?? base.apiKey,
    apiUrl: overrides.apiUrl ?? base.apiUrl,
    template: overrides.template ?? base.template,
    timeoutMs: overrides.timeoutMs ?? base.timeoutMs,
    containerImage: overrides.containerImage ?? base.containerImage,
    containerCli: overrides.containerCli ?? base.containerCli,
    containerRuntime: overrides.containerRuntime ?? base.containerRuntime,
    workspaceRoot: overrides.workspaceRoot ?? base.workspaceRoot,
    syncAfterExec: overrides.syncAfterExec ?? base.syncAfterExec,
    workspaceId: overrides.workspaceId ?? base.workspaceId,
  };
}
