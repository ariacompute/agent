/**
 * `Session` — a server-side agent session (`/v1/agents/sessions`) plus its
 * memory backend (`cloud` / `local` / `both`).
 *
 * Mirrors the Agents SDK session idea: it carries the conversation state so the
 * next `run()` continues where the previous one stopped.
 */

import type { ClientOptions, HistoryInput, MemoryBackend, MemoryConfig, MemoryOptions } from "./types.js";
import type { Agent } from "./agent.js";
import { getJson, postJson, resolveClient } from "./transport.js";
import {
  CloudMemoryStore,
  CompositeMemoryStore,
  LocalMemoryStore,
  type LocalMemoryOptions,
  type MemoryStore,
} from "./memory.js";

interface SessionResponse {
  id: string;
  object?: string;
  status?: string;
}

export class Session {
  readonly id: string;
  readonly history: HistoryInput[];
  readonly client: ClientOptions;
  /** Backend used when `memorize` / `recall` are called without an override. */
  readonly backend: MemoryBackend;
  private readonly local?: MemoryStore;
  private readonly cloud?: MemoryStore;
  private readonly defaultStore: MemoryStore;

  constructor(
    id: string,
    options: MemoryConfig = {},
    history: HistoryInput[] = [],
  ) {
    this.id = id;
    this.client = options;
    this.history = history;
    this.backend = options.backend ?? "cloud";
    this.local = new LocalMemoryStore(options as LocalMemoryOptions);
    this.cloud = new CloudMemoryStore(id, options);
    this.defaultStore = this.resolve(this.backend);
  }

  /**
   * Create a server-side session bound to `agent`, inheriting the agent's
   * memory backend configuration.
   *
   * The agent must already exist in the cloud (by id or by name).
   */
  static async create(agent: Agent, client: ClientOptions = {}): Promise<Session> {
    const resolved = resolveClient({ ...agent.client, ...client });
    const body: Record<string, unknown> = {};
    if (agent.id) {
      body.agent_id = agent.id;
    } else {
      body.agent = agent.name;
    }
    if (agent.instructions) {
      body.instructions = agent.instructions;
    }
    const res = await postJson<SessionResponse>(resolved, "/v1/agents/sessions", body);
    return new Session(res.id, { ...agent.memory, ...client });
  }

  private resolve(backend: MemoryBackend): MemoryStore {
    switch (backend) {
      case "local":
        return this.local!;
      case "cloud":
        return this.cloud!;
      case "both":
        return new CompositeMemoryStore(this.local!, this.cloud!);
    }
  }

  private storeFor(backend?: MemoryBackend): MemoryStore {
    if (!backend) return this.defaultStore;
    if (!["cloud", "local", "both"].includes(backend)) {
      throw new Error(`unknown memory backend: ${backend}`);
    }
    return this.resolve(backend);
  }

  /**
   * Write a keyed long-term memory.
   *
   * The value is persisted in the session's context store — Postgres + pgvector
   * for `cloud`, the aria memo (SQLite) store for `local` — and is recallable by
   * every later turn of the session. `both` writes to both.
   */
  async memorize(key: string, value: string, options: MemoryOptions = {}): Promise<void> {
    await this.storeFor(options.backend).put(key, value);
  }

  /** Read a keyed long-term memory (missing keys resolve to `null`). */
  async recall(key: string, options: MemoryOptions = {}): Promise<string | null> {
    return this.storeFor(options.backend).get(key);
  }

  /** Append an exchange to the client-side history mirror. */
  record(input: HistoryInput, output: string): void {
    this.history.push(input, { role: "assistant", content: output });
  }
}
