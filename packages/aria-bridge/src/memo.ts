import { loadConfig, type AriaBridgeConfig } from "./config.ts";
import { AriaError, ErrorCode } from "./error.ts";
import { defaultRunCommand, type RunCommand } from "./spawn.ts";

export const MEMO_TYPES = [
  "working",
  "short_term",
  "long_term:episodic",
  "long_term:semantic",
  "long_term:entity",
  "long_term:graph",
] as const;

export type MemoTypeName = (typeof MEMO_TYPES)[number];

export interface MemoClientOptions {
  config?: AriaBridgeConfig;
  runCommand?: RunCommand;
  signal?: AbortSignal;
}

export interface SearchHit {
  score: number;
  content: string;
}

export interface ListedMemo {
  id: string;
  typeLabel: string;
  content: string;
}

export interface MemoRecord {
  id: string;
  [key: string]: unknown;
}

function isMemoType(value: string): value is MemoTypeName {
  return (MEMO_TYPES as readonly string[]).includes(value);
}

async function runMemo(
  args: string[],
  options: MemoClientOptions,
): Promise<string> {
  const config = options.config ?? loadConfig();
  const run = options.runCommand ?? defaultRunCommand;
  const result = await run(config.memoBin, ["--db", config.memoDb, ...args], {
    signal: options.signal,
  });
  if (result.code !== 0) {
    const detail = (result.stderr || result.stdout).trim() || `exit ${result.code}`;
    throw new AriaError(`aria-memo failed: ${detail}`, ErrorCode.MEMO_CLI);
  }
  return result.stdout.replace(/\n+$/, "");
}

export async function memoAdd(
  input: { content: string; type?: string; importance?: number },
  options: MemoClientOptions = {},
): Promise<string> {
  const content = input.content;
  if (content.trim().length === 0) {
    throw new AriaError("memo content must not be empty", ErrorCode.EMPTY_CONTENT);
  }
  const type = input.type ?? "working";
  if (!isMemoType(type)) {
    throw new AriaError(`unknown memo_type: ${type}`, ErrorCode.INVALID_PARAM);
  }
  const importance = input.importance ?? 0.5;
  if (!(importance >= 0 && importance <= 1)) {
    throw new AriaError(
      `importance must be in [0, 1], got ${importance}`,
      ErrorCode.INVALID_PARAM,
    );
  }
  const out = await runMemo(
    ["add", "--type", type, "--content", content, "--importance", String(importance)],
    options,
  );
  const id = out.trim();
  if (id.length === 0) {
    throw new AriaError("aria-memo add returned empty id", ErrorCode.MEMO_CLI);
  }
  return id;
}

export async function memoGet(
  id: string,
  options: MemoClientOptions = {},
): Promise<MemoRecord | null> {
  if (id.trim().length === 0) {
    throw new AriaError("memo id must not be empty", ErrorCode.INVALID_PARAM);
  }
  const out = await runMemo(["get", "--id", id], options);
  if (out.trim() === "not found") {
    return null;
  }
  try {
    return JSON.parse(out) as MemoRecord;
  } catch {
    throw new AriaError(`aria-memo get returned non-JSON: ${out}`, ErrorCode.MEMO_CLI);
  }
}

export async function memoSearch(
  input: { text: string; topK?: number },
  options: MemoClientOptions = {},
): Promise<SearchHit[]> {
  if (input.text.trim().length === 0) {
    throw new AriaError("search text must not be empty", ErrorCode.EMPTY_CONTENT);
  }
  const topK = input.topK ?? 5;
  if (!Number.isInteger(topK) || topK <= 0) {
    throw new AriaError(`top_k must be a positive integer, got ${topK}`, ErrorCode.INVALID_PARAM);
  }
  const out = await runMemo(
    ["search", "--text", input.text, "--top-k", String(topK)],
    options,
  );
  if (out.trim().length === 0) {
    return [];
  }
  return out.split("\n").flatMap((line) => {
    const tab = line.indexOf("\t");
    if (tab < 0) {
      return [];
    }
    const score = Number(line.slice(0, tab));
    if (!Number.isFinite(score)) {
      return [];
    }
    return [{ score, content: line.slice(tab + 1) }];
  });
}

export async function memoList(
  input: { type?: string } = {},
  options: MemoClientOptions = {},
): Promise<ListedMemo[]> {
  if (input.type !== undefined && !isMemoType(input.type)) {
    throw new AriaError(`unknown memo_type: ${input.type}`, ErrorCode.INVALID_PARAM);
  }
  const args = ["list"];
  if (input.type) {
    args.push("--type", input.type);
  }
  const out = await runMemo(args, options);
  if (out.trim().length === 0) {
    return [];
  }
  return out.split("\n").flatMap((line) => {
    const match = /^(\S+) \[(.*)\] (.*)$/.exec(line);
    if (!match) {
      return [];
    }
    return [{ id: match[1], typeLabel: match[2], content: match[3] }];
  });
}

export async function memoForget(
  id: string,
  options: MemoClientOptions = {},
): Promise<boolean> {
  if (id.trim().length === 0) {
    throw new AriaError("memo id must not be empty", ErrorCode.INVALID_PARAM);
  }
  const out = await runMemo(["forget", "--id", id], options);
  const text = out.trim();
  if (text === "forgotten") {
    return true;
  }
  if (text === "not found") {
    return false;
  }
  throw new AriaError(`unexpected forget output: ${text}`, ErrorCode.MEMO_CLI);
}

export function formatSearchHits(hits: SearchHit[]): string {
  if (hits.length === 0) {
    return "(no memo hits)";
  }
  return hits.map((h) => `- (${h.score.toFixed(3)}) ${h.content}`).join("\n");
}
