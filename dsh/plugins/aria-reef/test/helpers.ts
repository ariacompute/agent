import type { Context } from "@deepseek-ai/cordis";
import type { AgentRecord, Feedback } from "../src/types.ts";

export interface FakeCtx {
  ctx: Context;
  tools: Array<{ name: string }>;
  listeners: Map<string, Array<(...args: unknown[]) => unknown>>;
  emit(event: string, ...args: unknown[]): Promise<unknown>;
}

/** Minimal cordis stand-in: records registrations, no runtime behaviour. */
export function createFakeCtx(): FakeCtx {
  const tools: Array<{ name: string }> = [];
  const listeners = new Map<string, Array<(...args: unknown[]) => unknown>>();
  const ctx = {
    llm: { registerAdapter: () => undefined },
    tools: {
      register: (def: { name: string }) => {
        tools.push(def);
        return def;
      },
    },
    on: (event: string, listener: (...args: unknown[]) => unknown) => {
      const list = listeners.get(event) ?? [];
      list.push(listener);
      listeners.set(event, list);
      return () => undefined;
    },
  } as unknown as Context;

  return {
    ctx,
    tools,
    listeners,
    async emit(event, ...args) {
      const list = listeners.get(event) ?? [];
      let last: unknown = args[0];
      for (const listener of list) {
        last = await listener(...args);
      }
      return last;
    },
  };
}

export interface FakeTimers {
  setTimer: (fn: () => void, ms: number) => unknown;
  clearTimer: (handle: unknown) => void;
  now: () => number;
  /** Advance the virtual clock, firing every timer due within the window. */
  advance(ms: number): Promise<void>;
  /** Number of armed timers. */
  pending(): number;
}

/**
 * Controllable clock + timer pair for the auto-cycle scheduler.
 * Uses no real sleeping: `advance` fires due callbacks and flushes microtasks.
 */
export function createFakeTimers(start = 0): FakeTimers {
  let current = start;
  let seq = 0;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const flush = (): Promise<void> => new Promise((resolve) => setTimeout(resolve, 0));

  return {
    setTimer(fn, ms) {
      seq += 1;
      timers.set(seq, { at: current + Math.max(0, ms), fn });
      return seq;
    },
    clearTimer(handle) {
      timers.delete(handle as number);
    },
    now() {
      return current;
    },
    pending() {
      return timers.size;
    },
    async advance(ms) {
      const target = current + ms;
      for (;;) {
        let nextId: number | undefined;
        let nextAt = Number.POSITIVE_INFINITY;
        for (const [id, timer] of timers) {
          if (timer.at <= target && timer.at < nextAt) {
            nextAt = timer.at;
            nextId = id;
          }
        }
        if (nextId === undefined) {
          break;
        }
        current = Math.max(current, nextAt);
        const timer = timers.get(nextId)!;
        timers.delete(nextId);
        timer.fn();
        await flush();
      }
      current = target;
    },
  };
}

export function makeRecord(patch: Partial<AgentRecord> = {}): AgentRecord {
  return {
    recordId: patch.recordId ?? "rec_test",
    createdAt: patch.createdAt ?? "2026-01-01T00:00:00.000Z",
    release: patch.release ?? "dev",
    request: patch.request ?? { messages: [{ role: "user", content: "hello" }] },
    response: patch.response,
    outcome: patch.outcome,
    trainedAt: patch.trainedAt,
    meta: patch.meta,
  };
}

export function makeFeedback(patch: Partial<Feedback> = {}): Feedback {
  return {
    feedbackId: patch.feedbackId ?? "fb_test",
    recordId: patch.recordId ?? "rec_test",
    source: patch.source ?? "user",
    score: patch.score,
    rawScore: patch.rawScore,
    text: patch.text,
    structured: patch.structured,
    createdAt: patch.createdAt ?? "2026-01-01T00:00:00.000Z",
    eligible: patch.eligible ?? true,
  };
}
