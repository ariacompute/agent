import { appendFile, mkdir, readFile, rename, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { AgentRecord, Feedback } from "./types.ts";

/** Persistence for captured turns. */
export interface RecordStore {
  append(record: AgentRecord): Promise<void>;
  get(recordId: string): Promise<AgentRecord | null>;
  update(recordId: string, patch: Partial<AgentRecord>): Promise<AgentRecord | null>;
  list(options?: { limit?: number; untrainedOnly?: boolean }): Promise<AgentRecord[]>;
}

/** Persistence for feedback bound to records. */
export interface FeedbackStore {
  append(feedback: Feedback): Promise<void>;
  forRecord(recordId: string): Promise<Feedback[]>;
  forRecords(recordIds: string[]): Promise<Feedback[]>;
  list(options?: { limit?: number }): Promise<Feedback[]>;
}

export interface ReefStore {
  records: RecordStore;
  feedback: FeedbackStore;
}

function storeError(message: string, cause: unknown): AriaError {
  const detail = cause instanceof Error ? cause.message : String(cause);
  return new AriaError(`reef store: ${message}: ${detail}`, ErrorCode.REEF_STORE);
}

// ---------------------------------------------------------------------------
// In-memory implementation (tests + offline runs)
// ---------------------------------------------------------------------------

export function createMemoryStore(): ReefStore {
  const records = new Map<string, AgentRecord>();
  const feedback: Feedback[] = [];

  return {
    records: {
      async append(record) {
        records.set(record.recordId, { ...record });
      },
      async get(recordId) {
        const found = records.get(recordId);
        return found ? { ...found } : null;
      },
      async update(recordId, patch) {
        const found = records.get(recordId);
        if (!found) {
          return null;
        }
        const merged: AgentRecord = { ...found, ...patch, recordId: found.recordId };
        records.set(recordId, merged);
        return { ...merged };
      },
      async list(options = {}) {
        let items = [...records.values()];
        if (options.untrainedOnly) {
          items = items.filter((item) => !item.trainedAt);
        }
        items.reverse();
        return options.limit && options.limit > 0 ? items.slice(0, options.limit) : items;
      },
    },
    feedback: {
      async append(item) {
        feedback.push({ ...item });
      },
      async forRecord(recordId) {
        return feedback.filter((item) => item.recordId === recordId).map((item) => ({ ...item }));
      },
      async forRecords(recordIds) {
        const wanted = new Set(recordIds);
        return feedback.filter((item) => wanted.has(item.recordId)).map((item) => ({ ...item }));
      },
      async list(options = {}) {
        const items = [...feedback].reverse();
        return options.limit && options.limit > 0 ? items.slice(0, options.limit) : items;
      },
    },
  };
}

// ---------------------------------------------------------------------------
// JSONL file implementation (default)
// ---------------------------------------------------------------------------

async function readJsonl<T>(file: string): Promise<T[]> {
  let raw: string;
  try {
    raw = await readFile(file, "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return [];
    }
    throw storeError(`read ${file}`, err);
  }
  const items: T[] = [];
  const lines = raw.split("\n");
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i]!.trim();
    if (line.length === 0) {
      continue;
    }
    try {
      items.push(JSON.parse(line) as T);
    } catch (err) {
      throw new AriaError(
        `reef store: corrupt line ${i + 1} in ${file}: ${err instanceof Error ? err.message : String(err)}`,
        ErrorCode.REEF_STORE,
      );
    }
  }
  return items;
}

async function appendJsonl(file: string, value: unknown): Promise<void> {
  try {
    await mkdir(join(file, ".."), { recursive: true });
    await appendFile(file, `${JSON.stringify(value)}\n`, "utf8");
  } catch (err) {
    throw storeError(`append ${file}`, err);
  }
}

async function rewriteJsonl(file: string, values: unknown[]): Promise<void> {
  const tmp = `${file}.tmp`;
  try {
    await mkdir(join(file, ".."), { recursive: true });
    await writeFile(tmp, values.map((value) => JSON.stringify(value)).join("\n") + (values.length ? "\n" : ""), "utf8");
    await rename(tmp, file);
  } catch (err) {
    throw storeError(`rewrite ${file}`, err);
  }
}

export interface FileStoreOptions {
  /** Override file names (defaults: records.jsonl / feedback.jsonl). */
  recordsFile?: string;
  feedbackFile?: string;
}

export function createFileStore(dir: string, options: FileStoreOptions = {}): ReefStore {
  const recordsFile = options.recordsFile ?? join(dir, "records.jsonl");
  const feedbackFile = options.feedbackFile ?? join(dir, "feedback.jsonl");

  return {
    records: {
      async append(record) {
        await appendJsonl(recordsFile, record);
      },
      async get(recordId) {
        const items = await readJsonl<AgentRecord>(recordsFile);
        return items.find((item) => item.recordId === recordId) ?? null;
      },
      async update(recordId, patch) {
        const items = await readJsonl<AgentRecord>(recordsFile);
        const index = items.findIndex((item) => item.recordId === recordId);
        if (index < 0) {
          return null;
        }
        const merged: AgentRecord = { ...items[index]!, ...patch, recordId };
        items[index] = merged;
        await rewriteJsonl(recordsFile, items);
        return merged;
      },
      async list(opts = {}) {
        let items = await readJsonl<AgentRecord>(recordsFile);
        if (opts.untrainedOnly) {
          items = items.filter((item) => !item.trainedAt);
        }
        items.reverse();
        return opts.limit && opts.limit > 0 ? items.slice(0, opts.limit) : items;
      },
    },
    feedback: {
      async append(item) {
        await appendJsonl(feedbackFile, item);
      },
      async forRecord(recordId) {
        const items = await readJsonl<Feedback>(feedbackFile);
        return items.filter((item) => item.recordId === recordId);
      },
      async forRecords(recordIds) {
        const wanted = new Set(recordIds);
        const items = await readJsonl<Feedback>(feedbackFile);
        return items.filter((item) => wanted.has(item.recordId));
      },
      async list(opts = {}) {
        const items = (await readJsonl<Feedback>(feedbackFile)).reverse();
        return opts.limit && opts.limit > 0 ? items.slice(0, opts.limit) : items;
      },
    },
  };
}
