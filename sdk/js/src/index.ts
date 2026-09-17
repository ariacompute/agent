/**
 * `@ariacompute/agent` — the Aria agent SDK.
 *
 * The API mirrors the OpenAI Agents SDK so the official quickstart transfers
 * unchanged:
 *
 * ```ts
 * import { Agent, run, tool } from "@ariacompute/agent";
 *
 * const agent = new Agent({
 *   name: "History tutor",
 *   instructions: "Answer history questions clearly and concisely.",
 *   model: "gpt-4o-mini",
 * });
 *
 * const result = await run(agent, "When did the Roman Empire fall?");
 * console.log(result.finalOutput);
 * ```
 */

export { Agent } from "./agent.js";
export { tool } from "./tool.js";
export { Session } from "./session.js";
export {
  CloudMemoryStore,
  CompositeMemoryStore,
  LocalMemoryStore,
  decodeLocal,
  encodeLocal,
  type MemoryStore,
} from "./memory.js";
export { run, runStreamed, decodeEvent } from "./runner.js";
export {
  resolveClient,
  postJson,
  getJson,
  postJsonStream,
  readSse,
  DEFAULT_BETA_HEADER,
} from "./transport.js";
export type {
  AgentConfig,
  ClientOptions,
  MemoryBackend,
  MemoryConfig,
  MemoryOptions,
  HistoryInput,
  HistoryItem,
  Role,
  RunResult,
  StreamEvent,
  StreamedRunResult,
  ToolParameters,
  ToolSpec,
} from "./types.js";
