import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { clamp01 } from "./feedback.ts";
import { promptOf } from "./record.ts";
import type { AgentRecord, ArtifactKind, Candidate, EvalResult, EvalScore, EvalTask, Feedback } from "./types.ts";

export interface RunnerInput {
  content: string;
  task: EvalTask;
  kind: ArtifactKind;
}

/** Scores one artifact version against one task (0..1, higher is better). */
export type EvalRunner = (input: RunnerInput) => Promise<number> | number;

const STOPWORDS = new Set([
  "that",
  "this",
  "with",
  "from",
  "should",
  "would",
  "could",
  "about",
  "into",
  "when",
  "then",
  "than",
  "their",
  "there",
  "here",
  "have",
  "been",
  "were",
  "what",
  "which",
  "please",
  "always",
  "never",
]);

/** Cheap deterministic runner: coverage of feedback hints + basic sanity. */
export const defaultRunner: EvalRunner = ({ content, task }) => {
  const trimmed = content.trim();
  if (trimmed.length === 0) {
    return 0;
  }
  let score = 0.5;
  const hints = (task.hints ?? []).filter((hint) => hint.trim().length > 0);
  if (hints.length > 0) {
    const lowered = trimmed.toLowerCase();
    const hits = hints.filter((hint) => lowered.includes(hint.toLowerCase())).length;
    score += 0.5 * (hits / hints.length);
  } else {
    score += 0.5;
  }
  if (/\bTODO\b|\bPLACEHOLDER\b/i.test(trimmed)) {
    score -= 0.05;
  }
  if (trimmed.length > 200_000) {
    score -= 0.1;
  }
  return clamp01(score);
};

/** Derive evaluation probes from the records that triggered the cycle. */
export function tasksFromRecords(records: AgentRecord[], feedback: Feedback[]): EvalTask[] {
  const byRecord = new Map<string, Feedback[]>();
  for (const item of feedback) {
    const list = byRecord.get(item.recordId) ?? [];
    list.push(item);
    byRecord.set(item.recordId, list);
  }
  const tasks: EvalTask[] = [];
  for (const record of records) {
    const items = byRecord.get(record.recordId) ?? [];
    if (items.length === 0) {
      continue;
    }
    const input = promptOf(record.request);
    const hints = keywords(
      items
        .map((item) => item.text ?? "")
        .join(" "),
    );
    tasks.push({ id: record.recordId, input, hints });
  }
  if (tasks.length === 0) {
    tasks.push({ id: "default", input: "" });
  }
  return tasks;
}

/** Extract distinctive keywords out of free-form feedback text. */
export function keywords(text: string, limit = 5): string[] {
  const seen = new Set<string>();
  for (const word of text.toLowerCase().match(/[a-z][a-z0-9_-]{3,}/g) ?? []) {
    if (STOPWORDS.has(word)) {
      continue;
    }
    seen.add(word);
    if (seen.size >= limit) {
      break;
    }
  }
  return [...seen];
}

export interface EvaluateOptions {
  current: string;
  candidate: Candidate;
  tasks: EvalTask[];
  runner?: EvalRunner;
  /** Relative improvement required before the candidate replaces current. */
  minImprovement?: number;
}

/** Run both versions and keep the winner (ties keep the current version). */
export async function evaluateCandidate(options: EvaluateOptions): Promise<EvalResult> {
  const runner = options.runner ?? defaultRunner;
  const tasks = options.tasks.length > 0 ? options.tasks : [{ id: "default", input: "" }];
  const minImprovement = options.minImprovement ?? 0;
  const currentScores: EvalScore[] = [];
  const candidateScores: EvalScore[] = [];

  for (const task of tasks) {
    currentScores.push({ taskId: task.id, score: await runOne(runner, options.current, task, options.candidate.kind) });
    candidateScores.push({
      taskId: task.id,
      score: await runOne(runner, options.candidate.content, task, options.candidate.kind),
    });
  }

  const current = average(currentScores.map((item) => item.score));
  const candidate = average(candidateScores.map((item) => item.score));
  const winner = candidate > current + minImprovement ? "candidate" : "current";
  return {
    recipe: options.candidate.recipe,
    target: options.candidate.target,
    current,
    candidate,
    winner,
    currentScores,
    candidateScores,
  };
}

async function runOne(
  runner: EvalRunner,
  content: string,
  task: EvalTask,
  kind: ArtifactKind,
): Promise<number> {
  let score: number;
  try {
    score = await runner({ content, task, kind });
  } catch (err) {
    if (err instanceof AriaError) {
      throw err;
    }
    throw new AriaError(
      `reef: eval runner failed on task ${task.id}: ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.REEF,
    );
  }
  if (typeof score !== "number" || !Number.isFinite(score)) {
    throw new AriaError(
      `reef: eval runner returned a non-finite score for task ${task.id}`,
      ErrorCode.INVALID_PARAM,
    );
  }
  return clamp01(score);
}

function average(values: number[]): number {
  if (values.length === 0) {
    return 0;
  }
  return values.reduce((sum, value) => sum + value, 0) / values.length;
}
