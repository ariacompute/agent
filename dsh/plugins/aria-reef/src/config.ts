import { homedir } from "node:os";
import { join } from "node:path";

export const DEFAULT_STORE_DIR = "reef";
export const DEFAULT_ARTIFACT_DIR = "reef-artifacts";
export const DEFAULT_ARIAPIN_URL = "http://127.0.0.1:8001";
export const DEFAULT_ARIAPIN_TIMEOUT_MS = 30_000;
export const DEFAULT_BATCH_SIZE = 16;
export const DEFAULT_MIN_FEEDBACK = 1;
export const DEFAULT_RELEASE = "dev";
export const DEFAULT_RECIPES = ["skillclaw", "prompt", "rules"];
export const DEFAULT_CYCLE_INTERVAL_MS = 900_000;
export const DEFAULT_CYCLE_MIN_SIGNALS = 8;
export const DEFAULT_CYCLE_POLL_MS = 60_000;
/** maxBackoff defaults to `intervalMs * this`. */
export const DEFAULT_CYCLE_BACKOFF_FACTOR = 4;

/** Artifact families tracked by Git LFS. */
export const LFS_PATTERNS = ["*.bin", "*.safetensors", "*.gguf", "*.pt"];

export interface ReefConfig {
  /** Master switch. When false the plugin registers nothing. */
  enabled: boolean;
  /** Id of the currently served release (stamped on every record). */
  release: string;
  /** Directory holding records.jsonl / feedback.jsonl. */
  storeDir: string;
  /** Git repo that version-controls evolved artifacts. */
  artifactRepo: string;
  /** Max records consumed per recipe per cycle. */
  batchSize: number;
  /** Minimum feedback items before a record becomes eligible. */
  minFeedback: number;
  /** Run the automatic (LLM-free) rubric scorer. */
  rubricEnabled: boolean;
  /** Publish accepted candidates without manual approval. */
  autoApply: boolean;
  /** Enabled recipe names, in order. */
  recipes: string[];
  /** ariapin base URL for weight training. */
  ariapinUrl: string;
  /** ariapin API key (Bearer). */
  ariapinKey: string;
  ariapinTimeoutMs: number;
  /** Local model bundle for in-process engine proposals (harness recipes). */
  bundle?: string;
  /** Native FFI lib path for `@ariacompute/engine-ts`. */
  ffiLib?: string;
  /** Model id passed to the engine. */
  model?: string;
  /** Base model id for weight training via ariapin (`ARIA_REEF_BASE_MODEL`). */
  baseModel?: string;
  /** Run improvement cycles automatically on a timer (`ARIA_REEF_CYCLE`). */
  autoCycle: boolean;
  /** Automatic cycle period (`ARIA_REEF_CYCLE_INTERVAL_MS`). */
  cycleIntervalMs: number;
  /** Trigger early once this many new signals accumulated; 0 disables (`ARIA_REEF_CYCLE_MIN_SIGNALS`). */
  cycleMinSignals: number;
  /** Tick period used to evaluate the triggers (`ARIA_REEF_CYCLE_POLL_MS`). */
  cyclePollMs: number;
  /** Upper bound of the failure backoff (`ARIA_REEF_CYCLE_MAX_BACKOFF_MS`). */
  cycleMaxBackoffMs: number;
}

export function defaultStoreDir(): string {
  return join(homedir(), ".ariacompute", "agent", DEFAULT_STORE_DIR);
}

export function defaultArtifactDir(): string {
  return join(homedir(), ".ariacompute", "agent", DEFAULT_ARTIFACT_DIR);
}

function parseBool(raw: string | undefined, fallback: boolean): boolean {
  if (raw === undefined) {
    return fallback;
  }
  const value = raw.trim().toLowerCase();
  if (value.length === 0) {
    return fallback;
  }
  if (/^(1|true|yes|on)$/.test(value)) {
    return true;
  }
  if (/^(0|false|no|off)$/.test(value)) {
    return false;
  }
  return fallback;
}

function parsePositiveInt(raw: string | undefined, fallback: number): number {
  if (raw === undefined) {
    return fallback;
  }
  const value = Number(raw.trim());
  return Number.isFinite(value) && value > 0 ? Math.floor(value) : fallback;
}

function parseNonNegativeInt(raw: string | undefined, fallback: number): number {
  if (raw === undefined) {
    return fallback;
  }
  const value = Number(raw.trim());
  return Number.isFinite(value) && value >= 0 ? Math.floor(value) : fallback;
}

function parseList(raw: string | undefined, fallback: string[]): string[] {
  if (raw === undefined) {
    return [...fallback];
  }
  const items = raw
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item.length > 0);
  return items.length > 0 ? items : [...fallback];
}

export function loadReefConfig(
  env: Record<string, string | undefined> = process.env,
): ReefConfig {
  const cycleIntervalMs = parsePositiveInt(env.ARIA_REEF_CYCLE_INTERVAL_MS, DEFAULT_CYCLE_INTERVAL_MS);
  const cyclePollMs = parseNonNegativeInt(env.ARIA_REEF_CYCLE_POLL_MS, 0);
  const cycleMaxBackoffMs = parseNonNegativeInt(env.ARIA_REEF_CYCLE_MAX_BACKOFF_MS, 0);
  return {
    enabled: parseBool(env.ARIA_REEF_ENABLED, false),
    release: env.ARIA_REEF_RELEASE?.trim() || DEFAULT_RELEASE,
    storeDir: env.ARIA_REEF_STORE_DIR?.trim() || defaultStoreDir(),
    artifactRepo: env.ARIA_REEF_ARTIFACT_REPO?.trim() || defaultArtifactDir(),
    batchSize: parsePositiveInt(env.ARIA_REEF_BATCH_SIZE, DEFAULT_BATCH_SIZE),
    minFeedback: parsePositiveInt(env.ARIA_REEF_MIN_FEEDBACK, DEFAULT_MIN_FEEDBACK),
    rubricEnabled: parseBool(env.ARIA_REEF_RUBRIC, true),
    autoApply: parseBool(env.ARIA_REEF_AUTO_APPLY, false),
    recipes: parseList(env.ARIA_REEF_RECIPES, DEFAULT_RECIPES),
    ariapinUrl: (env.ARIA_REEF_ARIAPIN_URL?.trim() || DEFAULT_ARIAPIN_URL).replace(/\/+$/, ""),
    ariapinKey: env.ARIA_REEF_ARIAPIN_KEY?.trim() ?? "",
    ariapinTimeoutMs: parsePositiveInt(env.ARIA_REEF_ARIAPIN_TIMEOUT_MS, DEFAULT_ARIAPIN_TIMEOUT_MS),
    bundle: env.ARIA_REEF_BUNDLE?.trim() || env.ARIA_MODEL_BUNDLE?.trim() || undefined,
    ffiLib: env.ARIA_REEF_FFI_LIB?.trim() || env.ARIA_FFI_LIB?.trim() || undefined,
    model: env.ARIA_REEF_MODEL?.trim() || env.ARIA_ENGINE_MODEL?.trim() || undefined,
    baseModel: env.ARIA_REEF_BASE_MODEL?.trim() || undefined,
    autoCycle: parseBool(env.ARIA_REEF_CYCLE, false),
    cycleIntervalMs,
    cycleMinSignals: parseNonNegativeInt(env.ARIA_REEF_CYCLE_MIN_SIGNALS, DEFAULT_CYCLE_MIN_SIGNALS),
    cyclePollMs: cyclePollMs > 0 ? cyclePollMs : Math.min(cycleIntervalMs, DEFAULT_CYCLE_POLL_MS),
    cycleMaxBackoffMs:
      cycleMaxBackoffMs > 0 ? cycleMaxBackoffMs : cycleIntervalMs * DEFAULT_CYCLE_BACKOFF_FACTOR,
  };
}

export function withOverrides(base: ReefConfig, overrides: Partial<ReefConfig> = {}): ReefConfig {
  return {
    enabled: overrides.enabled ?? base.enabled,
    release: overrides.release ?? base.release,
    storeDir: overrides.storeDir ?? base.storeDir,
    artifactRepo: overrides.artifactRepo ?? base.artifactRepo,
    batchSize: overrides.batchSize ?? base.batchSize,
    minFeedback: overrides.minFeedback ?? base.minFeedback,
    rubricEnabled: overrides.rubricEnabled ?? base.rubricEnabled,
    autoApply: overrides.autoApply ?? base.autoApply,
    recipes: overrides.recipes ?? base.recipes,
    ariapinUrl: overrides.ariapinUrl ?? base.ariapinUrl,
    ariapinKey: overrides.ariapinKey ?? base.ariapinKey,
    ariapinTimeoutMs: overrides.ariapinTimeoutMs ?? base.ariapinTimeoutMs,
    bundle: overrides.bundle ?? base.bundle,
    ffiLib: overrides.ffiLib ?? base.ffiLib,
    model: overrides.model ?? base.model,
    baseModel: overrides.baseModel ?? base.baseModel,
    autoCycle: overrides.autoCycle ?? base.autoCycle,
    cycleIntervalMs: overrides.cycleIntervalMs ?? base.cycleIntervalMs,
    cycleMinSignals: overrides.cycleMinSignals ?? base.cycleMinSignals,
    cyclePollMs: overrides.cyclePollMs ?? base.cyclePollMs,
    cycleMaxBackoffMs: overrides.cycleMaxBackoffMs ?? base.cycleMaxBackoffMs,
  };
}
