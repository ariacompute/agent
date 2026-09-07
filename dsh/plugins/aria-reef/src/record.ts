import type { Context } from "@deepseek-ai/cordis";
import { newId, type AgentRecord, type Outcome } from "./types.ts";
import type { ReefStore } from "./store.ts";

export interface RecorderOptions {
  store: ReefStore;
  /** Release id stamped on every record. */
  release: string;
  now?: () => Date;
  /** Failure sink — reef never throws into the agent loop. */
  logger?: (message: string) => void;
}

export interface Recorder {
  /** Build + persist a record for one turn. Persistence is detached. */
  start(input: unknown): AgentRecord;
  /** Attach the response / outcome once the turn finished. */
  complete(recordId: string, response: unknown, outcome?: Outcome): Promise<AgentRecord | null>;
  /** Fire-and-forget variant used by the post-step hook (never blocks the loop). */
  completeDetached(recordId: string, response: unknown, outcome?: Outcome): void;
  lastRecordId(): string | undefined;
  /** Await all in-flight writes (tests + graceful shutdown). */
  flush(): Promise<void>;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Best-effort extraction of the user prompt from a pre-step payload. */
export function promptOf(request: unknown): string {
  if (!isObject(request)) {
    return typeof request === "string" ? request : "";
  }
  if (typeof request.prompt === "string" && request.prompt.trim()) {
    return request.prompt;
  }
  const messages = request.messages;
  if (!Array.isArray(messages)) {
    return "";
  }
  for (let i = messages.length - 1; i >= 0; i -= 1) {
    const message = messages[i] as { role?: unknown; content?: unknown };
    if (message?.role !== "user") {
      continue;
    }
    const text = contentToText(message.content);
    if (text.trim()) {
      return text;
    }
  }
  return "";
}

function contentToText(content: unknown): string {
  if (typeof content === "string") {
    return content;
  }
  if (!Array.isArray(content)) {
    return "";
  }
  return content
    .map((block) => {
      if (block && typeof block === "object" && "text" in block && typeof (block as { text?: unknown }).text === "string") {
        return (block as { text: string }).text;
      }
      return "";
    })
    .join("");
}

/** Infer an outcome from a post-step payload (no signal -> unknown). */
export function outcomeFromResponse(value: unknown): Outcome {
  if (!isObject(value)) {
    return "unknown";
  }
  if (value.error !== undefined && value.error !== null) {
    return "failure";
  }
  const finish = value.finish as { reason?: { kind?: unknown } } | undefined;
  const kind = finish?.reason?.kind;
  if (kind === "error" || kind === "aborted") {
    return "failure";
  }
  if (kind === "stop" || kind === "tool-calls") {
    return "success";
  }
  const outcome = value.outcome;
  if (outcome === "success" || outcome === "failure") {
    return outcome;
  }
  return "unknown";
}

/** Attach the record receipt to the pre-step payload without dropping fields. */
export function attachReceipt(input: unknown, recordId: string): unknown {
  if (!isObject(input)) {
    return input;
  }
  const existing = isObject(input.reef) ? input.reef : {};
  return { ...input, reef: { ...existing, recordId } };
}

export function recordIdOf(value: unknown): string | undefined {
  if (!isObject(value)) {
    return undefined;
  }
  if (typeof value.recordId === "string") {
    return value.recordId;
  }
  const reef = value.reef;
  if (isObject(reef) && typeof reef.recordId === "string") {
    return reef.recordId;
  }
  return undefined;
}

export function createRecorder(options: RecorderOptions): Recorder {
  const now = options.now ?? (() => new Date());
  const logger = options.logger ?? ((message: string) => console.warn(`[aria-reef] ${message}`));
  const pending = new Set<Promise<unknown>>();
  let last: string | undefined;

  function track(work: Promise<unknown>): void {
    const wrapped = work.catch((err: unknown) => {
      logger(`record persistence failed: ${err instanceof Error ? err.message : String(err)}`);
    });
    pending.add(wrapped);
    void wrapped.finally(() => pending.delete(wrapped));
  }

  async function complete(recordId: string, response: unknown, outcome?: Outcome): Promise<AgentRecord | null> {
    const patch: Partial<AgentRecord> = { response };
    patch.outcome = outcome ?? outcomeFromResponse(response);
    const updated = await options.store.records.update(recordId, patch);
    if (!updated) {
      logger(`record ${recordId} not found; outcome discarded`);
      return null;
    }
    return updated;
  }

  return {
    start(input) {
      const record: AgentRecord = {
        recordId: newId("rec"),
        createdAt: now().toISOString(),
        release: options.release,
        request: input,
      };
      last = record.recordId;
      track(options.store.records.append(record));
      return record;
    },
    complete,
    completeDetached(recordId, response, outcome) {
      track(complete(recordId, response, outcome));
    },
    lastRecordId() {
      return last;
    },
    async flush() {
      while (pending.size > 0) {
        await Promise.allSettled([...pending]);
      }
    },
  };
}

/**
 * Register Serve hooks: every turn gets a `record_id` receipt and is persisted
 * off the critical path. Any failure is logged, never thrown into the agent loop.
 */
export function registerServeHooks(ctx: Context, recorder: Recorder): void {
  ctx.on("agent/pre-step", (...args: unknown[]) => {
    const value = args[0];
    const next = args[1] as ((v?: unknown) => unknown) | undefined;
    let forwarded = value;
    try {
      const record = recorder.start(value);
      forwarded = attachReceipt(value, record.recordId);
    } catch (err) {
      console.warn(
        `[aria-reef] failed to start record: ${err instanceof Error ? err.message : String(err)}`,
      );
    }
    return next ? next(forwarded) : forwarded;
  });

  ctx.on("agent/post-step", (...args: unknown[]) => {
    const value = args[0];
    const next = args[1] as ((v?: unknown) => unknown) | undefined;
    const recordId = recordIdOf(value) ?? recorder.lastRecordId();
    if (recordId) {
      // Detached: the outcome write must never delay or break the agent turn.
      recorder.completeDetached(recordId, value);
    }
    return next ? next(value) : value;
  });
}
