/**
 * `Session` — a server-side agent session (`/v1/agents/sessions`).
 *
 * Mirrors the Agents SDK session idea: it carries the conversation state so the
 * next `run()` continues where the previous one stopped.
 */

import type { ClientOptions, HistoryInput } from "./types.js";
import type { Agent } from "./agent.js";
import { getJson, postJson, resolveClient } from "./transport.js";

interface SessionResponse {
  id: string;
  object?: string;
  status?: string;
}

export class Session {
  readonly id: string;
  readonly history: HistoryInput[];
  readonly client: ClientOptions;

  constructor(id: string, client: ClientOptions = {}, history: HistoryInput[] = []) {
    this.id = id;
    this.client = client;
    this.history = history;
  }

  /**
   * Create a server-side session bound to `agent`.
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
    return new Session(res.id, { ...agent.client, ...client });
  }

  /**
   * Write a keyed long-term memory for this session.
   *
   * The value is persisted in the session's context store (Postgres + pgvector
   * on the cloud, the on-device store for native SDKs) and is recallable by
   * every later turn of the session — even after a restart.
   */
  async memorize(key: string, value: string): Promise<void> {
    const resolved = resolveClient(this.client);
    await postJson(
      resolved,
      `/v1/agents/sessions/${encodeURIComponent(this.id)}/memory`,
      { key, value },
    );
  }

  /** Read a keyed long-term memory (missing keys resolve to `null`). */
  async recall(key: string): Promise<string | null> {
    const resolved = resolveClient(this.client);
    const res = await getJson<{ key: string; value?: string | null }>(
      resolved,
      `/v1/agents/sessions/${encodeURIComponent(this.id)}/memory/${encodeURIComponent(key)}`,
    );
    return res.value ?? null;
  }

  /** Append an exchange to the client-side history mirror. */
  record(input: HistoryInput, output: string): void {
    this.history.push(input, { role: "assistant", content: output });
  }
}
