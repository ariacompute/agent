/**
 * Shared types for `@ariacompute/agent`.
 *
 * The names intentionally mirror the OpenAI Agents SDK (`@openai/agents`) so a
 * quickstart written against it transfers unchanged: `Agent`, `run`,
 * `runStreamed`, `tool`, `Session`, `result.finalOutput`, `result.history`.
 */

export interface ClientOptions {
  /** Base URL of the aria-agent-cloud service (e.g. `http://localhost:3000`). */
  baseUrl?: string;
  /** API key sent as `Authorization: Bearer <key>`. */
  apiKey?: string;
  /** Override the default `fetch` implementation (tests, custom agents). */
  fetch?: typeof globalThis.fetch;
  /** Beta header required by the OpenAI-compatible surface. */
  betaHeader?: string;
}

/** A JSON-schema-ish description of a tool's parameters. */
export type ToolParameters = Record<string, unknown>;

export interface ToolSpec {
  name: string;
  description?: string;
  parameters?: ToolParameters;
  /** Local implementation — only used by hosted/local runners, never by the cloud. */
  execute?: (args: any) => unknown | Promise<unknown>;
}

export interface AgentConfig {
  name: string;
  /** Appended to the agent's base instructions (mirrors `instructions`). */
  instructions?: string;
  model?: string;
  tools?: ToolSpec[];
  /** Specialist agents this agent may hand off to. */
  handoffs?: Agent[];
  /** Description used when this agent is offered as a handoff target. */
  handoffDescription?: string;
  /** Existing agent id (skips name-based lookup). */
  id?: string;
  /** Client configuration used when this agent runs. */
  client?: ClientOptions;
}

export type Role = "user" | "assistant" | "system";

export interface HistoryItem {
  role: Role;
  content: string;
}

/** One entry of the run history (mirrors `result.history` in the Agents SDK). */
export type HistoryInput = HistoryItem | string;

/** Streaming events decoded from the SSE `agent.*` frames. */
export type StreamEvent =
  | { type: "agent.turn.created"; turnId?: string; raw: Record<string, any> }
  | { type: "agent.turn.in_progress"; phase?: string; label?: string | null; raw: Record<string, any> }
  | { type: "agent.turn.item.added"; item?: Record<string, any>; raw: Record<string, any> }
  | { type: "agent.turn.item.done"; item?: Record<string, any>; raw: Record<string, any> }
  | { type: "agent.turn.output_text.delta"; delta: string; raw: Record<string, any> }
  | { type: "agent.turn.output_text.done"; text: string; raw: Record<string, any> }
  | { type: "agent.turn.completed"; output?: string; raw: Record<string, any> }
  | { type: "agent.turn.failed"; error?: string; raw: Record<string, any> }
  | { type: string; raw: Record<string, any> };

/** Result of a completed run (mirrors `RunResult` in the Agents SDK). */
export interface RunResult {
  /** The assistant's final reply. */
  finalOutput: string;
  /** Conversation history including the input and the reply. */
  history: HistoryInput[];
  /** Agent that produced the reply (last handoff target, if any). */
  lastAgent?: Agent;
  /** Server-side session id (reuse it to continue the conversation). */
  sessionId?: string;
  /** Server-side turn id. */
  turnId?: string;
}

/** Handle returned by `runStreamed`. */
export interface StreamedRunResult {
  /** Async iterable of decoded streaming events. */
  events: AsyncIterable<StreamEvent>;
  /** Resolves once the terminal `agent.turn.completed` / `agent.turn.failed` arrives. */
  completed: Promise<RunResult>;
}
