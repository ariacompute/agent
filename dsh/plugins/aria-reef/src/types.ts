/** Core data contracts shared by the Serve / Observe / Grow / Commit / Surface stages. */

/** Artifact families the reef loop can evolve. */
export type ArtifactKind = "skill" | "prompt" | "rules" | "weight";

/** Recipe families: harness edits (local engine only) vs model weights (ariapin). */
export type RecipeKind = "harness" | "weight";

export type Outcome = "success" | "failure" | "unknown";

export type FeedbackSource = "user" | "task_outcome" | "rubric";

/** One agent turn captured by the Serve stage. */
export interface AgentRecord {
  recordId: string;
  createdAt: string;
  /** Release (version) id that produced this turn — used for replay comparisons. */
  release: string;
  /** Raw `agent/pre-step` payload (messages / prompt). */
  request: unknown;
  /** Response or tool result, filled in asynchronously after the turn. */
  response?: unknown;
  outcome?: Outcome;
  /** Set once the record has been consumed by a Grow cycle. */
  trainedAt?: string;
  meta?: Record<string, unknown>;
}

/** Feedback bound to a record. `score` is normalized to 0..1. */
export interface Feedback {
  feedbackId: string;
  recordId: string;
  source: FeedbackSource;
  score?: number;
  /** Score as supplied by the user (e.g. 1..5) before normalization. */
  rawScore?: number;
  text?: string;
  structured?: Record<string, unknown>;
  createdAt: string;
  /** Whether the owning record qualifies for training at append time. */
  eligible: boolean;
}

/** A proposed artifact change produced by a recipe. */
export interface Candidate {
  recipe: string;
  recipeKind: RecipeKind;
  kind: ArtifactKind;
  /** Artifact path inside the artifact repo, e.g. `skill/search`. */
  target: string;
  /** New full artifact content (weight: serialized training-job ref). */
  content: string;
  /** Unified diff against the current version (human readable). */
  diff: string;
  score?: number;
  notes?: string;
  meta?: Record<string, unknown>;
}

/** Location of an evolvable artifact. */
export interface ArtifactRef {
  kind: ArtifactKind;
  name: string;
}

/** One evaluation probe run against both the current and the candidate version. */
export interface EvalTask {
  id: string;
  input: string;
  expected?: string;
  /** Hints (e.g. keywords taken from feedback) the artifact should address. */
  hints?: string[];
}

export interface EvalScore {
  taskId: string;
  score: number;
  detail?: string;
}

export interface EvalResult {
  recipe: string;
  target: string;
  current: number;
  candidate: number;
  winner: "current" | "candidate";
  currentScores: EvalScore[];
  candidateScores: EvalScore[];
}

/** Result of one Grow -> Evaluate -> Commit pass for a single recipe. */
export interface RecipeCycleResult {
  recipe: string;
  kind: RecipeKind;
  target: string;
  eligible: boolean;
  skipReason?: string;
  candidate?: Candidate;
  evaluation?: EvalResult;
  applied: boolean;
  version?: number;
  commit?: string | null;
  error?: string;
}

export interface CycleReport {
  release: string;
  scanned: number;
  results: RecipeCycleResult[];
}

let counter = 0;

/** Monotonic-ish id with a readable prefix. Not crypto-grade. */
export function newId(prefix: string, now: () => number = Date.now): string {
  counter += 1;
  return `${prefix}_${now().toString(36)}${counter.toString(36)}`;
}

export function artifactKey(ref: ArtifactRef): string {
  return `${ref.kind}/${ref.name}`;
}
