/**
 * Memory backends for `@ariacompute/agent`.
 *
 * * `cloud` — the agent-cloud session memory REST endpoints
 *   (`/v1/agents/sessions/{id}/memory`).
 * * `local` — **aria memo** (SQLite), reached through the `aria-memo` CLI
 *   (`--json`). The CLI call is an injectable seam (`exec`) so tests never need
 *   the binary installed.
 * * `both` — writes to both and reads merged + deduped (cloud copy wins).
 */

import { getJson, postJson, resolveClient } from "./transport.js";
import type { ClientOptions, MemoryBackend } from "./types.js";

/// Runs `aria-memo <args…>` and resolves with stdout.
export type MemoExec = (args: string[]) => Promise<string>;

export interface MemoryStore {
  readonly backend: MemoryBackend;
  put(key: string, value: string): Promise<void>;
  get(key: string): Promise<string | null>;
}

export interface LocalMemoryOptions extends ClientOptions {
  /** `aria-memo` binary (default `aria-memo`, or `ARIA_MEMO_BIN`). */
  memoBin?: string;
  /** aria memo database path (default `memo.db`, or `ARIA_MEMO_DB`). */
  memoDb?: string;
  /** Injectable CLI runner (tests). */
  exec?: MemoExec;
}

/** Value convention shared by every SDK: a keyed local memory is stored as `key: value`. */
export function encodeLocal(key: string, value: string): string {
  return `${key}: ${value}`;
}

/** Strip the `key: ` prefix written by {@link encodeLocal}; unknown shapes pass through. */
export function decodeLocal(key: string, content: string): string {
  const prefix = `${key}: `;
  return content.startsWith(prefix) ? content.slice(prefix.length) : content;
}

/**
 * Default `aria-memo` runner: spawns the CLI and resolves with its stdout.
 *
 * The `node:child_process` import is dynamic so bundlers targeting browsers /
 * edge runtimes never pull it in; the callback params are typed explicitly
 * because `strict` rejects the implicit `any` they would otherwise get.
 */
function defaultExec(bin: string): MemoExec {
  return async (args: string[]) => {
    const { spawn } = await import("node:child_process");
    return new Promise<string>((resolve, reject) => {
      const child = spawn(bin, args, { stdio: ["ignore", "pipe", "pipe"] });
      let out = "";
      let err = "";
      // `stdio: pipe` guarantees both streams exist, but they are typed nullable.
      const stdout = child.stdout;
      const stderr = child.stderr;
      stdout?.setEncoding("utf8");
      stderr?.setEncoding("utf8");
      stdout?.on("data", (chunk: string) => {
        out += chunk;
      });
      stderr?.on("data", (chunk: string) => {
        err += chunk;
      });
      child.on("error", reject);
      child.on("close", (code: number | null) => {
        if (code === 0) {
          resolve(out);
        } else {
          reject(
            new Error(
              `aria-memo exited with ${code}: ${err.trim() || out.trim()}. Install it with \`aria-memo setup\` / \`aria-memo upgrade\`.`,
            ),
          );
        }
      });
    });
  };
}

function parseJsonLines(raw: string): Record<string, any>[] {
  const text = raw.trim();
  if (!text) return [];
  const parsed = JSON.parse(text);
  return Array.isArray(parsed) ? parsed : [parsed];
}

/**
 * `local` backend: aria memo, driven by the `aria-memo` CLI.
 *
 * `put` writes `key: value` as a `long_term:semantic` memory so the entry is
 * visible in `aria-memo list`; `get` searches by the key and strips the prefix.
 */
export class LocalMemoryStore implements MemoryStore {
  readonly backend: MemoryBackend = "local";
  private readonly bin: string;
  private readonly db: string;
  private readonly run: MemoExec;

  constructor(options: LocalMemoryOptions = {}) {
    this.bin =
      options.memoBin ?? process.env.ARIA_MEMO_BIN ?? "aria-memo";
    this.db = options.memoDb ?? process.env.ARIA_MEMO_DB ?? "memo.db";
    this.run = options.exec ?? defaultExec(this.bin);
  }

  private args(command: string[]): string[] {
    return ["--db", this.db, ...command];
  }

  async put(key: string, value: string): Promise<void> {
    await this.run(
      this.args([
        "add",
        "--type",
        "long_term:semantic",
        "--content",
        encodeLocal(key, value),
        "--importance",
        "0.8",
      ]),
    );
  }

  async get(key: string): Promise<string | null> {
    const raw = await this.run(this.args(["search", "--text", key, "--top-k", "5", "--json"]));
    const rows = parseJsonLines(raw);
    const hit = rows.find((r) => typeof r.content === "string" && r.content.startsWith(`${key}: `));
    const content = hit?.content ?? rows[0]?.content;
    return typeof content === "string" ? decodeLocal(key, content) : null;
  }
}

/** `cloud` backend: the agent-cloud session memory REST endpoints. */
export class CloudMemoryStore implements MemoryStore {
  readonly backend: MemoryBackend = "cloud";
  private readonly client: ClientOptions;
  private readonly sessionId: string;

  constructor(sessionId: string, client: ClientOptions = {}) {
    this.sessionId = sessionId;
    this.client = client;
  }

  private base(): string {
    return `/v1/agents/sessions/${encodeURIComponent(this.sessionId)}/memory`;
  }

  async put(key: string, value: string): Promise<void> {
    const resolved = resolveClient(this.client);
    await postJson(resolved, this.base(), { key, value, kind: "long_term" });
  }

  async get(key: string): Promise<string | null> {
    const resolved = resolveClient(this.client);
    const res = await getJson<{ value?: string | null }>(
      resolved,
      `${this.base()}/${encodeURIComponent(key)}`,
    );
    return res.value ?? null;
  }
}

/**
 * `both` backend: writes to local **and** cloud, reads merged.
 *
 * A single-side failure is tolerated (the other copy still answers); both
 * failing surfaces the error. The cloud copy wins when both have the key.
 */
export class CompositeMemoryStore implements MemoryStore {
  readonly backend: MemoryBackend = "both";
  private readonly local: MemoryStore;
  private readonly cloud: MemoryStore;

  constructor(local: MemoryStore, cloud: MemoryStore) {
    this.local = local;
    this.cloud = cloud;
  }

  async put(key: string, value: string): Promise<void> {
    const results = await Promise.allSettled([
      this.local.put(key, value),
      this.cloud.put(key, value),
    ]);
    if (results.every((r) => r.status === "rejected")) {
      throw new Error(
        `memory write failed on every backend: ${results
          .map((r) => (r.status === "rejected" ? String(r.reason) : ""))
          .filter(Boolean)
          .join("; ")}`,
      );
    }
  }

  async get(key: string): Promise<string | null> {
    const [local, cloud] = await Promise.allSettled([
      this.local.get(key),
      this.cloud.get(key),
    ]);
    const cloudValue = cloud.status === "fulfilled" ? cloud.value : null;
    if (cloudValue != null) return cloudValue;
    const localValue = local.status === "fulfilled" ? local.value : null;
    if (localValue != null) return localValue;
    if (local.status === "rejected" && cloud.status === "rejected") {
      throw new Error(
        `memory read failed on every backend: ${String(local.reason)}; ${String(cloud.reason)}`,
      );
    }
    return null;
  }
}
