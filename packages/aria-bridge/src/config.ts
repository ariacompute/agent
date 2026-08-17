import { homedir } from "node:os";
import { join } from "node:path";

export const DEFAULT_ENGINE_URL = "http://127.0.0.1:8080/v1";
export const DEFAULT_MEMO_BIN = "aria-memo";

export interface AriaBridgeConfig {
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
  const memoBin = env.ARIA_MEMO_BIN?.trim() || DEFAULT_MEMO_BIN;
  const memoDb = env.ARIA_MEMO_DB?.trim() || defaultMemoDb();
  return {
    engineUrl: normalizeEngineUrl(engineRaw),
    memoBin,
    memoDb,
  };
}
