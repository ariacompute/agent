/**
 * `run` / `runStreamed` — mirrors the OpenAI Agents SDK runner entry points.
 *
 * Both drive one turn of the agent against the cloud's beta Agents API:
 * `POST /v1/agents/sessions/{id}/events` (blocking) or
 * `POST /v1/agents/sessions/{id}/events/stream` (SSE).
 */

import type {
  ClientOptions,
  HistoryInput,
  RunResult,
  StreamEvent,
  StreamedRunResult,
} from "./types.js";
import type { Agent } from "./agent.js";
import { Session } from "./session.js";
import { postJson, postJsonStream, resolveClient } from "./transport.js";

interface TurnResponse {
  id: string;
  object?: string;
  session_id?: string;
  agent_id?: string;
  status?: string;
  output?: string | null;
}

function toText(input: HistoryInput | HistoryInput[]): string {
  if (typeof input === "string") return input;
  if (Array.isArray(input)) {
    return input.map((i) => (typeof i === "string" ? i : i.content)).join("\n");
  }
  return input.content;
}

/** Ensure we have a session id for this run, creating one when needed. */
async function ensureSession(
  agent: Agent,
  client: ClientOptions,
  session?: Session | string,
): Promise<Session> {
  if (session instanceof Session) return session;
  if (typeof session === "string" && session.length > 0) {
    return new Session(session, client);
  }
  return Session.create(agent, client);
}

/**
 * Run one turn to completion.
 *
 * ```ts
 * const result = await run(agent, "When did Rome fall?");
 * console.log(result.finalOutput);
 * ```
 */
export async function run(
  agent: Agent,
  input: HistoryInput | HistoryInput[],
  options: { session?: Session | string; client?: ClientOptions } = {},
): Promise<RunResult> {
  const client = { ...agent.client, ...options.client };
  const resolved = resolveClient(client);
  const session = await ensureSession(agent, client, options.session);
  const text = toText(input);

  const turn = await postJson<TurnResponse>(
    resolved,
    `/v1/agents/sessions/${encodeURIComponent(session.id)}/events`,
    { input: text },
  );
  const output = turn.output ?? "";
  session.record(input as HistoryInput, output);

  return {
    finalOutput: output,
    history: session.history,
    lastAgent: agent,
    sessionId: session.id,
    turnId: turn.id,
  };
}

/**
 * Run one turn, streaming events as they arrive.
 *
 * ```ts
 * const streamed = await runStreamed(agent, "Tell me something surprising");
 * for await (const ev of streamed.events) {
 *   if (ev.type === "agent.turn.output_text.delta") process.stdout.write(ev.delta);
 * }
 * const result = await streamed.completed;
 * console.log(result.finalOutput);
 * ```
 */
export async function runStreamed(
  agent: Agent,
  input: HistoryInput | HistoryInput[],
  options: { session?: Session | string; client?: ClientOptions } = {},
): Promise<StreamedRunResult> {
  const client = { ...agent.client, ...options.client };
  const resolved = resolveClient(client);
  const session = await ensureSession(agent, client, options.session);
  const text = toText(input);

  const frames = postJsonStream(
    resolved,
    `/v1/agents/sessions/${encodeURIComponent(session.id)}/events/stream`,
    { input: text },
  );

  let collected = "";
  let turnId: string | undefined;
  let failed: string | undefined;

  // The upstream SSE body is single-consumer, so it is drained once into a
  // buffer that both `events` and `completed` can iterate independently.
  const buffered: Record<string, any>[] = [];
  let drainPromise: Promise<void> | null = null;

  function drain(): Promise<void> {
    if (drainPromise) return drainPromise;
    drainPromise = (async () => {
      await consume();
    })();
    return drainPromise;
  }

  async function consume(): Promise<void> {
    for await (const frame of frames) {
      buffered.push(frame);
      turnId = (frame.turn_id as string) ?? turnId;
      switch (frame.type) {
        case "agent.turn.output_text.delta":
          collected += String(frame.delta ?? "");
          break;
        case "agent.turn.output_text.done":
        case "agent.turn.completed": {
          const out = frame.text ?? (frame.turn && frame.turn.output);
          if (typeof out === "string" && out.length > 0) {
            collected = out;
          }
          break;
        }
        case "agent.turn.failed":
          failed = (frame.error && frame.error.message) ?? "run failed";
          break;
        default:
          break;
      }
    }
  }

  async function* events(): AsyncGenerator<StreamEvent> {
    await drain();
    for (const frame of buffered) {
      yield decodeEvent(frame);
    }
  }

  const completed = (async (): Promise<RunResult> => {
    // Draining here means `completed` resolves even when `events` is ignored.
    await drain();
    if (failed) {
      throw new Error(failed);
    }
    session.record(input as HistoryInput, collected);
    return {
      finalOutput: collected,
      history: session.history,
      lastAgent: agent,
      sessionId: session.id,
      turnId,
    };
  })();

  return { events: events(), completed };
}

/** Narrow a raw frame to the typed union (unknown types pass through). */
export function decodeEvent(frame: Record<string, any>): StreamEvent {
  const type = String(frame.type ?? "");
  switch (type) {
    case "agent.turn.created":
      return { type, turnId: frame.turn_id, raw: frame };
    case "agent.turn.in_progress":
      return { type, phase: frame.phase, label: frame.label ?? null, raw: frame };
    case "agent.turn.item.added":
    case "agent.turn.item.done":
      return { type, item: frame.item, raw: frame };
    case "agent.turn.output_text.delta":
      return { type, delta: String(frame.delta ?? ""), raw: frame };
    case "agent.turn.output_text.done":
      return { type, text: String(frame.text ?? ""), raw: frame };
    case "agent.turn.completed":
      return { type, output: frame.turn?.output, raw: frame };
    case "agent.turn.failed":
      return { type, error: frame.error?.message, raw: frame };
    default:
      return { type, raw: frame };
  }
}
