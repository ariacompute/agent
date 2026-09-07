import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { promptOf } from "./record.ts";
import type { SchedulerStatus } from "./scheduler.ts";
import type { ReefStore } from "./store.ts";
import { newId, type AgentRecord, type Feedback, type FeedbackSource, type Outcome } from "./types.ts";

/** Scores may be given on a 0..1 or a 1..5 scale; 1..5 is normalized. */
export const FEEDBACK_SCALE = 5;

export function clamp01(value: number): number {
  if (!Number.isFinite(value)) {
    return 0;
  }
  return Math.min(1, Math.max(0, value));
}

export function normalizeScore(raw: number): number {
  if (!Number.isFinite(raw)) {
    throw new AriaError(`feedback score must be a finite number, got ${raw}`, ErrorCode.INVALID_PARAM);
  }
  return raw > 1 ? clamp01(raw / FEEDBACK_SCALE) : clamp01(raw);
}

export interface FeedbackOptions {
  store: ReefStore;
  now?: () => Date;
}

function baseFeedback(
  options: FeedbackOptions,
  recordId: string,
  source: FeedbackSource,
): Feedback {
  return {
    feedbackId: newId("fb"),
    recordId,
    source,
    createdAt: (options.now ?? (() => new Date()))().toISOString(),
    eligible: false,
  };
}

/** Task-outcome signal: success -> 1, failure -> 0, unknown -> no score. */
export function feedbackFromOutcome(
  options: FeedbackOptions,
  recordId: string,
  outcome: Outcome,
): Feedback {
  const feedback = baseFeedback(options, recordId, "task_outcome");
  feedback.structured = { outcome };
  if (outcome === "success") {
    feedback.score = 1;
    feedback.rawScore = 1;
  } else if (outcome === "failure") {
    feedback.score = 0;
    feedback.rawScore = 0;
  }
  return feedback;
}

export function feedbackFromRubric(
  options: FeedbackOptions,
  recordId: string,
  score: number,
): Feedback {
  const feedback = baseFeedback(options, recordId, "rubric");
  feedback.score = clamp01(score);
  feedback.rawScore = score;
  feedback.structured = { rubric: "default" };
  return feedback;
}

export function feedbackFromUser(
  options: FeedbackOptions,
  input: { recordId: string; score?: number; text?: string; structured?: Record<string, unknown> },
): Feedback {
  const feedback = baseFeedback(options, input.recordId, "user");
  if (input.score !== undefined) {
    feedback.rawScore = input.score;
    feedback.score = normalizeScore(input.score);
  }
  if (input.text !== undefined) {
    feedback.text = input.text;
  }
  if (input.structured !== undefined) {
    feedback.structured = input.structured;
  }
  return feedback;
}

// ---------------------------------------------------------------------------
// Eligibility
// ---------------------------------------------------------------------------

export interface EligibilityPolicy {
  /** Minimum number of feedback items bound to a record. */
  minFeedback?: number;
  /** Require at least one numeric score. */
  requireScore?: boolean;
}

export function isEligible(
  record: AgentRecord,
  feedback: Feedback[],
  policy: EligibilityPolicy = {},
): boolean {
  if (record.trainedAt) {
    return false;
  }
  const min = policy.minFeedback ?? 1;
  const own = feedback.filter((item) => item.recordId === record.recordId);
  const usable = own.filter((item) => item.score !== undefined || (item.text?.trim().length ?? 0) > 0);
  if (usable.length < min) {
    return false;
  }
  if ((policy.requireScore ?? true) && !usable.some((item) => item.score !== undefined)) {
    return false;
  }
  return true;
}

export function meanScore(feedback: Feedback[]): number | undefined {
  const scores = feedback
    .map((item) => item.score)
    .filter((score): score is number => typeof score === "number");
  if (scores.length === 0) {
    return undefined;
  }
  return scores.reduce((sum, value) => sum + value, 0) / scores.length;
}

// ---------------------------------------------------------------------------
// Automatic rubric scoring (deterministic, no LLM)
// ---------------------------------------------------------------------------

export type Rubric = (record: AgentRecord) => number | null;

/**
 * Cheap offline rubric: rewards successful turns, penalizes error markers.
 * Returns null when the turn carries no signal at all.
 */
export const defaultRubric: Rubric = (record) => {
  if (record.response === undefined || record.response === null) {
    return null;
  }
  let score = record.outcome === "success" ? 0.8 : record.outcome === "failure" ? 0.1 : 0.5;
  let serialized: string;
  try {
    serialized = JSON.stringify(record.response);
  } catch {
    serialized = String(record.response);
  }
  if (serialized.length === 0 || serialized === "{}" || serialized === '""') {
    return null;
  }
  if (/\b(error|exception|failed|traceback)\b/i.test(serialized)) {
    score -= 0.2;
  }
  return clamp01(score);
};

/** Score every unscored record and persist rubric feedback. Returns new items. */
export async function applyRubric(
  options: FeedbackOptions,
  rubric: Rubric = defaultRubric,
  policy: { limit?: number } = {},
): Promise<Feedback[]> {
  const records = await options.store.records.list({ limit: policy.limit, untrainedOnly: true });
  const created: Feedback[] = [];
  for (const record of records) {
    const existing = await options.store.feedback.forRecord(record.recordId);
    if (existing.some((item) => item.source === "rubric")) {
      continue;
    }
    const score = rubric(record);
    if (score === null) {
      continue;
    }
    const feedback = feedbackFromRubric(options, record.recordId, score);
    await options.store.feedback.append(feedback);
    created.push(feedback);
  }
  return created;
}

/** Compact textual digest of feedback used to steer artifact edits. */
export function summarizeFeedback(records: AgentRecord[], feedback: Feedback[]): string {
  const byRecord = new Map<string, Feedback[]>();
  for (const item of feedback) {
    const list = byRecord.get(item.recordId) ?? [];
    list.push(item);
    byRecord.set(item.recordId, list);
  }
  const lines: string[] = [];
  for (const record of records) {
    const items = byRecord.get(record.recordId) ?? [];
    if (items.length === 0) {
      continue;
    }
    const score = meanScore(items);
    const texts = items
      .map((item) => item.text?.trim())
      .filter((text): text is string => Boolean(text))
      .slice(0, 3);
    const prompt = promptOf(record.request).slice(0, 200);
    lines.push(
      `- record ${record.recordId}${score !== undefined ? ` score=${score.toFixed(2)}` : ""}` +
        `${prompt ? ` prompt="${prompt}"` : ""}` +
        (texts.length ? ` feedback="${texts.join(" | ")}"` : ""),
    );
  }
  return lines.join("\n");
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

export interface ReportToolContext {
  store: ReefStore;
  /** Fallback record when the caller omits `record_id`. */
  lastRecordId?: () => string | undefined;
  now?: () => Date;
  policy?: EligibilityPolicy;
}

/** `aria_reef_report` — explicit user feedback bound to a record. */
export function createReportTool(ctxDeps: ReportToolContext) {
  const options: FeedbackOptions = { store: ctxDeps.store, now: ctxDeps.now };
  return {
    name: "aria_reef_report",
    description:
      "Report feedback for an agent turn so the reef loop can learn from it. " +
      "Score accepts 0..1 or 1..5 (1..5 is normalized).",
    parameters: {
      record_id: { type: "string", description: "Record id (defaults to the latest turn)" },
      score: { type: "number", description: "Numeric score: 0..1 or 1..5" },
      text: { type: "string", description: "Free-form feedback text" },
      structured: { type: "string", description: "Optional JSON object with structured feedback" },
    },
    output: {
      schema: { type: "object" },
      render: (_args: unknown, value: { recordId: string }) => [
        { type: "text", text: JSON.stringify(value) },
      ],
    },
    async execute(
      args: { record_id?: string; score?: number; text?: string; structured?: string },
    ): Promise<{ recordId: string; feedbackId: string; score?: number; eligible: boolean }> {
      if (args.score === undefined && !args.text?.trim() && !args.structured?.trim()) {
        throw new AriaError(
          "aria_reef_report requires at least one of score / text / structured",
          ErrorCode.INVALID_PARAM,
        );
      }
      const recordId = args.record_id?.trim() || ctxDeps.lastRecordId?.();
      if (!recordId) {
        throw new AriaError(
          "aria_reef_report: no record_id given and no recent turn recorded",
          ErrorCode.INVALID_PARAM,
        );
      }
      const record = await ctxDeps.store.records.get(recordId);
      if (!record) {
        throw new AriaError(`aria_reef_report: unknown record_id ${recordId}`, ErrorCode.INVALID_PARAM);
      }
      let structured: Record<string, unknown> | undefined;
      if (args.structured?.trim()) {
        try {
          const parsed: unknown = JSON.parse(args.structured);
          if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
            throw new Error("expected a JSON object");
          }
          structured = parsed as Record<string, unknown>;
        } catch (err) {
          throw new AriaError(
            `aria_reef_report: structured must be a JSON object: ${err instanceof Error ? err.message : String(err)}`,
            ErrorCode.INVALID_PARAM,
          );
        }
      }
      const feedback = feedbackFromUser(options, {
        recordId,
        score: args.score,
        text: args.text,
        structured,
      });
      const existing = await ctxDeps.store.feedback.forRecord(recordId);
      feedback.eligible = isEligible(record, [...existing, feedback], ctxDeps.policy);
      await ctxDeps.store.feedback.append(feedback);
      return {
        recordId,
        feedbackId: feedback.feedbackId,
        score: feedback.score,
        eligible: feedback.eligible,
      };
    },
  };
}

export interface StatusToolContext {
  store: ReefStore;
  release: string;
  recipes: string[];
  /** Auto-cycle scheduler state; omitted from the output when absent. */
  scheduler?: () => SchedulerStatus;
}

/** `aria_reef_status` — observability into the reef loop state. */
export function createStatusTool(ctxDeps: StatusToolContext) {
  return {
    name: "aria_reef_status",
    description: "Show reef loop counters: stored records, feedback items, release and enabled recipes.",
    parameters: {},
    output: {
      schema: { type: "object" },
      render: (_args: unknown, value: unknown) => [{ type: "text", text: JSON.stringify(value) }],
    },
    async execute(): Promise<{
      release: string;
      records: number;
      pending: number;
      feedback: number;
      recipes: string[];
      lastRecordId?: string;
      autoCycle?: SchedulerStatus;
    }> {
      const records = await ctxDeps.store.records.list();
      const pending = await ctxDeps.store.records.list({ untrainedOnly: true });
      const feedback = await ctxDeps.store.feedback.list();
      const auto = ctxDeps.scheduler?.();
      return {
        release: ctxDeps.release,
        records: records.length,
        pending: pending.length,
        feedback: feedback.length,
        recipes: [...ctxDeps.recipes],
        lastRecordId: records[0]?.recordId,
        // Conditional spread: callers without a scheduler keep the original shape.
        ...(auto ? { autoCycle: auto } : {}),
      };
    },
  };
}
