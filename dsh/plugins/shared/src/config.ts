import { homedir } from "node:os";
import { join } from "node:path";

export const DEFAULT_ENGINE_URL = "http://127.0.0.1:8080/v1";
export const DEFAULT_MEMO_BIN = "aria-memo";

export interface AriaBridgeConfig {
  /** Local engine bundle path for in-process FFI via @ariacompute/engine-ts. */
  engineBundle?: string;
  /** Path to the native FFI shared library (ARIA_FFI_LIB). */
  engineFfiLib?: string;
  /** Default model id used by the in-process engine adapter. */
  defaultModel?: string;
  /** Optional OpenAI-compatible HTTP endpoint (fallback when no bundle is set). */
  engineUrl: string;
  memoBin: string;
  memoDb: string;
}

export function defaultMemoDb(): string {
  return join(homedir(), ".ariacompute", "memo.db");
}

/** Normalize to an OpenAI-compatible `/v1` root with no trailing slash. */
export function normalizeEngineUrl(raw: string): string {
  const trimmed = raw.trim().replace(/\/+$/, "");
  if (trimmed.length === 0) {
    return DEFAULT_ENGINE_URL;
  }
  return trimmed.endsWith("/v1") ? trimmed : `${trimmed}/v1`;
}

export function loadConfig(
  env: Record<string, string | undefined> = process.env,
): AriaBridgeConfig {
  const engineRaw = env.ARIA_ENGINE_URL?.trim() || DEFAULT_ENGINE_URL;
  const engineBundle = env.ARIA_ENGINE_BUNDLE?.trim() || undefined;
  const engineFfiLib = env.ARIA_FFI_LIB?.trim() || undefined;
  const defaultModel = env.ARIA_ENGINE_MODEL?.trim() || undefined;
  const memoBin = env.ARIA_MEMO_BIN?.trim() || DEFAULT_MEMO_BIN;
  const memoDb = env.ARIA_MEMO_DB?.trim() || defaultMemoDb();
  return {
    engineBundle,
    engineFfiLib,
    defaultModel,
    engineUrl: normalizeEngineUrl(engineRaw),
    memoBin,
    memoDb,
  };
}
