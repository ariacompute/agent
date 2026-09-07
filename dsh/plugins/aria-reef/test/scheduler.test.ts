import { describe, expect, it } from "bun:test";
import { createScheduler, SIGNAL_GATE_MS, type SchedulerConfig } from "../src/scheduler.ts";
import type { CycleReport } from "../src/types.ts";
import { createFakeTimers } from "./helpers.ts";

const report = {} as CycleReport;

function config(patch: Partial<SchedulerConfig> = {}): SchedulerConfig {
  return {
    intervalMs: 10_000,
    minSignals: 0,
    pollMs: 1_000,
    backoffBaseMs: 1_000,
    maxBackoffMs: 60_000,
    ...patch,
  };
}

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (err: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const tick = (ms = 0): Promise<void> => new Promise((resolve) => setTimeout(resolve, ms));

describe("createScheduler", () => {
  it("runs on the interval and not before", async () => {
    const timers = createFakeTimers();
    let runs = 0;
    const scheduler = createScheduler({
      cycle: async () => {
        runs += 1;
        return report;
      },
      countSignals: async () => 0,
      config: config({ intervalMs: 10_000, pollMs: 1_000, minSignals: 0 }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    scheduler.start();
    expect(scheduler.status().started).toBe(true);

    await timers.advance(9_000);
    expect(runs).toBe(0);

    await timers.advance(1_000);
    expect(runs).toBe(1);
    expect(scheduler.status().runs).toBe(1);

    await scheduler.stop();
    expect(scheduler.status().started).toBe(false);
  });

  it("is a no-op while disabled at the plugin level (no timer created)", () => {
    // Covered by plugin.test.ts (autoCycle defaults to off); here we assert the
    // scheduler itself only arms a timer after start().
    const timers = createFakeTimers();
    const scheduler = createScheduler({
      cycle: async () => report,
      countSignals: async () => 0,
      config: config(),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    expect(timers.pending()).toBe(0);
    scheduler.start();
    expect(timers.pending()).toBe(1);
    scheduler.start();
    expect(timers.pending()).toBe(1);
    void scheduler.stop();
  });

  it("triggers early on the signal threshold", async () => {
    const timers = createFakeTimers();
    let runs = 0;
    const scheduler = createScheduler({
      cycle: async () => {
        runs += 1;
        return report;
      },
      countSignals: async () => 8,
      config: config({ intervalMs: 1_000_000, minSignals: 8, pollMs: SIGNAL_GATE_MS }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    scheduler.start();
    await timers.advance(30_000);
    expect(runs).toBe(0);
    await timers.advance(30_000);
    expect(runs).toBe(1);
    void scheduler.stop();
  });

  it("ignores signals below the threshold", async () => {
    const timers = createFakeTimers();
    let runs = 0;
    const scheduler = createScheduler({
      cycle: async () => {
        runs += 1;
        return report;
      },
      countSignals: async () => 3,
      config: config({ intervalMs: 1_000_000, minSignals: 8, pollMs: 10_000 }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    scheduler.start();
    await timers.advance(120_000);
    expect(runs).toBe(0);
    expect(scheduler.status().runs).toBe(0);
    void scheduler.stop();
  });

  it("skips ticks while a cycle is in flight", async () => {
    const gate = deferred<CycleReport>();
    let started = 0;
    const scheduler = createScheduler({
      cycle: () => {
        started += 1;
        return gate.promise;
      },
      countSignals: async () => 0,
      config: config({ minSignals: 0 }),
      logger: () => undefined,
    });
    const first = scheduler.tick();
    expect(scheduler.status().running).toBe(true);

    const second = await scheduler.tick();
    expect(second).toEqual({ ran: false, reason: "busy" });
    expect(scheduler.status().skipped).toBe(1);
    expect(started).toBe(1);

    gate.resolve(report);
    await first;
    expect(scheduler.status().running).toBe(false);
    expect(scheduler.status().runs).toBe(1);
  });

  it("backs off on failures without stopping, and resets on success", async () => {
    const timers = createFakeTimers();
    let failing = true;
    const scheduler = createScheduler({
      cycle: async () => {
        if (failing) {
          throw new Error("boom");
        }
        return report;
      },
      countSignals: async () => 0,
      config: config({
        intervalMs: 1_000,
        pollMs: 100,
        backoffBaseMs: 100,
        maxBackoffMs: 1_000,
        minSignals: 0,
      }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    scheduler.start();
    await timers.advance(1_000);
    expect(scheduler.status().failures).toBe(1);
    expect(scheduler.status().nextDelayMs).toBe(100);

    // Backoff doubles and is capped by maxBackoffMs.
    await timers.advance(1_500);
    expect(scheduler.status().failures).toBe(5);
    expect(scheduler.status().nextDelayMs).toBe(1_000);
    expect(scheduler.status().lastError).toBe("boom");
    expect(scheduler.status().started).toBe(true);

    failing = false;
    await timers.advance(1_000);
    expect(scheduler.status().failures).toBe(0);
    expect(scheduler.status().runs).toBe(1);
    expect(scheduler.status().nextDelayMs).toBe(1_000);
    expect(scheduler.status().lastRunAt).toBeDefined();
    void scheduler.stop();
  });

  it("stop clears the timer and waits for the in-flight cycle", async () => {
    const timers = createFakeTimers();
    const gate = deferred<CycleReport>();
    const scheduler = createScheduler({
      cycle: () => gate.promise,
      countSignals: async () => 0,
      config: config({ intervalMs: 100, pollMs: 100, minSignals: 0 }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: () => undefined,
    });
    scheduler.start();
    void timers.advance(100);
    await tick();
    expect(scheduler.status().running).toBe(true);

    let stopped = false;
    const stopping = scheduler.stop().then(() => {
      stopped = true;
    });
    await tick();
    expect(stopped).toBe(false);
    expect(timers.pending()).toBe(0);
    expect(scheduler.status().started).toBe(false);

    gate.resolve(report);
    await stopping;
    expect(stopped).toBe(true);
    expect(scheduler.status().running).toBe(false);
    expect(scheduler.status().runs).toBe(1);
  });

  it("survives a failing signal counter", async () => {
    const timers = createFakeTimers();
    let runs = 0;
    const logs: string[] = [];
    const scheduler = createScheduler({
      cycle: async () => {
        runs += 1;
        return report;
      },
      countSignals: async () => {
        throw new Error("store down");
      },
      config: config({ intervalMs: 1_000, pollMs: 500, minSignals: 4 }),
      setTimer: timers.setTimer,
      clearTimer: timers.clearTimer,
      now: timers.now,
      logger: (message) => logs.push(message),
    });
    scheduler.start();
    await timers.advance(1_000);
    expect(runs).toBe(1);
    expect(logs.some((line) => line.includes("signal count failed"))).toBe(true);
    void scheduler.stop();
  });
});
