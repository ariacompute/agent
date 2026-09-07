import type { CycleReport } from "./types.ts";

/** Signal-threshold trigger is additionally gated by this window (ms). */
export const SIGNAL_GATE_MS = 60_000;

export interface SchedulerConfig {
  /** Run a cycle when this much time passed since the last attempt. */
  intervalMs: number;
  /** Run a cycle early once this many new signals accumulated (<=0 disables). */
  minSignals: number;
  /** Tick period: how often the trigger conditions are evaluated. */
  pollMs: number;
  /** Base delay for exponential backoff after failures. */
  backoffBaseMs: number;
  /** Upper bound for the backoff delay. */
  maxBackoffMs: number;
}

export interface SchedulerDeps {
  /** One reef cycle (Grow -> Evaluate -> Commit -> Surface). */
  cycle: () => Promise<CycleReport>;
  /** Number of untrained eligible records; injected so it stays offline-testable. */
  countSignals: () => Promise<number>;
  config: SchedulerConfig;
  setTimer?: (fn: () => void, ms: number) => unknown;
  clearTimer?: (handle: unknown) => void;
  now?: () => number;
  logger?: (message: string) => void;
}

export interface SchedulerStatus {
  started: boolean;
  running: boolean;
  /** Successful cycles. */
  runs: number;
  /** Ticks skipped because a cycle was still in flight. */
  skipped: number;
  /** Consecutive failures (reset on success). */
  failures: number;
  lastRunAt?: string;
  lastDurationMs?: number;
  lastError?: string;
  /** Delay currently required before the next interval trigger. */
  nextDelayMs: number;
}

export interface TickResult {
  ran: boolean;
  /** `busy` | `not-due` | `interval` | `signals`. */
  reason: string;
  signals?: number;
}

export interface Scheduler {
  start(): void;
  /** Clear the timer and wait for an in-flight cycle to settle. */
  stop(): Promise<void>;
  /** Evaluate the trigger conditions once (awaits the cycle when it fires). */
  tick(): Promise<TickResult>;
  status(): SchedulerStatus;
}

function message(err: unknown, max = 200): string {
  const text = err instanceof Error ? err.message : String(err);
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

const defaultSetTimer = (fn: () => void, ms: number): unknown => setTimeout(fn, ms);
const defaultClearTimer = (handle: unknown): void => {
  clearTimeout(handle as ReturnType<typeof setTimeout>);
};

/**
 * Automatic cycle scheduler: interval OR signal-threshold trigger, single flight
 * (a tick while a cycle runs is skipped), detached execution (the timer callback
 * never awaits), exponential backoff on failures and a graceful stop.
 */
export function createScheduler(deps: SchedulerDeps): Scheduler {
  const cfg = deps.config;
  const setTimer = deps.setTimer ?? defaultSetTimer;
  const clearTimer = deps.clearTimer ?? defaultClearTimer;
  const now = deps.now ?? (() => Date.now());
  const logger = deps.logger ?? ((msg: string) => console.warn(`[aria-reef] ${msg}`));

  let started = false;
  let running = false;
  let handle: unknown;
  let inflight: Promise<TickResult> | undefined;
  let runs = 0;
  let skipped = 0;
  let failures = 0;
  let lastRunAt: string | undefined;
  let lastDurationMs: number | undefined;
  let lastError: string | undefined;
  let lastAttemptAt: number | undefined;

  const nextDelay = (): number =>
    failures === 0
      ? cfg.intervalMs
      : Math.min(cfg.maxBackoffMs, cfg.backoffBaseMs * 2 ** (failures - 1));

  async function evaluate(): Promise<TickResult> {
    const nowMs = now();
    if (running) {
      skipped += 1;
      return { ran: false, reason: "busy" };
    }
    let signals: number | undefined;
    if (cfg.minSignals > 0) {
      try {
        signals = await deps.countSignals();
      } catch (err) {
        // Never let counting noise interrupt the loop; the interval trigger still applies.
        logger(`auto cycle signal count failed: ${message(err)}`);
      }
    }
    const delay = nextDelay();
    const since = lastAttemptAt === undefined ? Number.POSITIVE_INFINITY : nowMs - lastAttemptAt;
    const due = since >= delay;
    let signalDue = false;
    if (cfg.minSignals > 0 && signals !== undefined && signals >= cfg.minSignals) {
      // While backing off, failures win: do not short-circuit the penalty with signals.
      const gate = failures > 0 ? delay : Math.min(cfg.intervalMs, SIGNAL_GATE_MS);
      signalDue = since >= gate;
    }
    if (!due && !signalDue) {
      return { ran: false, reason: "not-due", ...(signals === undefined ? {} : { signals }) };
    }
    const reason = due ? "interval" : "signals";
    lastAttemptAt = nowMs;
    running = true;
    try {
      await deps.cycle();
      runs += 1;
      failures = 0;
      lastError = undefined;
      lastRunAt = new Date(now()).toISOString();
      lastDurationMs = now() - nowMs;
    } catch (err) {
      failures += 1;
      lastError = message(err);
      logger(`auto cycle failed (${failures} consecutive): ${lastError}`);
    } finally {
      running = false;
    }
    return { ran: true, reason, ...(signals === undefined ? {} : { signals }) };
  }

  /** Evaluate once, tracking the promise so `stop` can await it. */
  function tick(): Promise<TickResult> {
    const pending = evaluate();
    inflight = pending;
    void pending
      .catch((err: unknown) => logger(`auto cycle tick failed: ${message(err)}`))
      .finally(() => {
        if (inflight === pending) {
          inflight = undefined;
        }
      });
    return pending;
  }

  function runTick(): void {
    handle = undefined;
    // Detached: the timer callback never awaits the cycle.
    void tick();
    if (started) {
      scheduleNext();
    }
  }

  function scheduleNext(): void {
    if (!started) {
      return;
    }
    handle = setTimer(runTick, Math.max(1, Math.floor(cfg.pollMs)));
  }

  return {
    start() {
      if (started) {
        return;
      }
      started = true;
      lastAttemptAt ??= now();
      scheduleNext();
    },
    async stop() {
      started = false;
      if (handle !== undefined) {
        clearTimer(handle);
        handle = undefined;
      }
      const pending = inflight;
      if (pending) {
        try {
          await pending;
        } catch {
          // Already logged by runTick.
        }
      }
    },
    tick,
    status() {
      return {
        started,
        running,
        runs,
        skipped,
        failures,
        ...(lastRunAt === undefined ? {} : { lastRunAt }),
        ...(lastDurationMs === undefined ? {} : { lastDurationMs }),
        ...(lastError === undefined ? {} : { lastError }),
        nextDelayMs: nextDelay(),
      };
    },
  };
}
