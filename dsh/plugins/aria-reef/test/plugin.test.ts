import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunCommand } from "../../shared/src/index.ts";
import { apply } from "../src/index.ts";
import { createFakeProposer } from "../src/recipes/llm.ts";
import { createMemoryStore } from "../src/store.ts";
import { createFakeCtx, createFakeTimers, makeFeedback, makeRecord } from "./helpers.ts";

let dir: string;
const run: RunCommand = async (_command, args) => ({
  code: 0,
  stdout: args[2] === "rev-parse" ? "sha1\n" : "",
  stderr: "",
});

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "reef-plugin-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("aria-reef plugin", () => {
  it("registers nothing while disabled", () => {
    const fake = createFakeCtx();
    const plugin = apply(fake.ctx, {}, { store: createMemoryStore(), runCommand: run, logger: () => undefined });
    expect(plugin).toBeUndefined();
    expect(fake.tools.length).toBe(0);
    expect(fake.listeners.size).toBe(0);
  });

  it("registers tools and serve hooks when enabled", async () => {
    const store = createMemoryStore();
    const fake = createFakeCtx();
    const plugin = apply(
      fake.ctx,
      { enabled: true, artifactRepo: dir, release: "v1" },
      { store, runCommand: run, llm: createFakeProposer("x"), logger: () => undefined },
    );
    expect(plugin).toBeDefined();
    expect(fake.tools.map((tool) => tool.name)).toEqual([
      "aria_reef_report",
      "aria_reef_status",
      "aria_reef_cycle",
    ]);
    expect(fake.listeners.has("agent/pre-step")).toBe(true);

    const forwarded = (await fake.emit(
      "agent/pre-step",
      { messages: [{ role: "user", content: "hi" }] },
      (value?: unknown) => value,
    )) as { reef: { recordId: string } };
    await plugin!.recorder.flush();
    const stored = await store.records.get(forwarded.reef.recordId);
    expect(stored?.release).toBe("v1");
    expect(plugin!.config.enabled).toBe(true);
  });

  it("runs a full cycle: propose, evaluate, commit and surface", async () => {
    const store = createMemoryStore();
    await store.records.append(
      makeRecord({
        recordId: "r1",
        request: { messages: [{ role: "user", content: "do X" }] },
        response: "done",
      }),
    );
    await store.feedback.append(
      makeFeedback({ recordId: "r1", score: 0.2, text: "always include citations" }),
    );

    const fake = createFakeCtx();
    const plugin = apply(
      fake.ctx,
      {
        enabled: true,
        autoApply: true,
        recipes: ["skillclaw"],
        rubricEnabled: false,
        artifactRepo: dir,
        release: "v1",
      },
      {
        store,
        runCommand: run,
        llm: createFakeProposer("improved skill with citations"),
        logger: () => undefined,
      },
    );

    const report = await plugin!.cycle();
    expect(report.scanned).toBe(1);
    const [first, ...rest] = report.results;
    expect(first?.recipe).toBe("skillclaw");
    expect(first?.applied).toBe(true);
    expect(first?.version).toBe(1);
    expect(first?.evaluation?.winner).toBe("candidate");
    expect(rest.map((item) => item.skipReason)).toEqual([
      "recipe disabled by configuration",
      "recipe disabled by configuration",
    ]);

    const active = await plugin!.active({ kind: "skill", name: "skills/agent" });
    expect(active).toEqual({ version: 1, content: "improved skill with citations" });

    // Consumed records are not re-trained.
    expect((await plugin!.cycle()).scanned).toBe(0);
  });

  it("keeps the current version when autoApply is off", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1", response: "done" }));
    await store.feedback.append(makeFeedback({ recordId: "r1", score: 0.1, text: "add citations" }));
    const fake = createFakeCtx();
    const plugin = apply(
      fake.ctx,
      { enabled: true, autoApply: false, recipes: ["prompt"], rubricEnabled: false, artifactRepo: dir },
      { store, runCommand: run, llm: createFakeProposer("prompt with citations"), logger: () => undefined },
    );
    const report = await plugin!.cycle();
    const prompt = report.results.find((item) => item.recipe === "prompt");
    expect(prompt?.applied).toBe(false);
    expect(prompt?.skipReason).toBe("candidate won but autoApply is off");
    expect(await plugin!.active({ kind: "prompt", name: "prompts/system" })).toBeNull();
  });

  it("registers the weight recipe only with a base model", () => {
    const without = apply(
      createFakeCtx().ctx,
      { enabled: true, artifactRepo: dir, recipes: ["skillclaw", "weight"] },
      { store: createMemoryStore(), runCommand: run, llm: createFakeProposer("x"), logger: () => undefined },
    );
    expect(without?.registry.get("weight")).toBeUndefined();

    const withModel = apply(
      createFakeCtx().ctx,
      { enabled: true, artifactRepo: dir, recipes: ["weight"], weight: { baseModel: "base/model" } },
      { store: createMemoryStore(), runCommand: run, llm: createFakeProposer("x"), logger: () => undefined },
    );
    expect(withModel?.registry.get("weight")?.artifact).toBe("weight");
  });

  it("arms no timer while the auto cycle is disabled", () => {
    const timers = createFakeTimers();
    const plugin = apply(
      createFakeCtx().ctx,
      { enabled: true, artifactRepo: dir },
      {
        store: createMemoryStore(),
        runCommand: run,
        logger: () => undefined,
        setTimer: timers.setTimer,
        clearTimer: timers.clearTimer,
        now: timers.now,
      },
    );
    expect(plugin?.scheduler).toBeUndefined();
    expect(timers.pending()).toBe(0);
  });

  it("starts and stops the auto cycle scheduler when enabled", async () => {
    const timers = createFakeTimers();
    const fake = createFakeCtx();
    const plugin = apply(
      fake.ctx,
      {
        enabled: true,
        artifactRepo: dir,
        autoCycle: true,
        cycleIntervalMs: 1_000,
        cyclePollMs: 100,
        cycleMinSignals: 0,
        rubricEnabled: false,
      },
      {
        store: createMemoryStore(),
        runCommand: run,
        llm: createFakeProposer("x"),
        logger: () => undefined,
        setTimer: timers.setTimer,
        clearTimer: timers.clearTimer,
        now: timers.now,
      },
    );
    expect(plugin?.scheduler).toBeDefined();
    expect(timers.pending()).toBe(1);

    const statusTool = plugin!.tools[1] as { execute(): Promise<Record<string, unknown>> };
    const status = await statusTool.execute();
    expect(status.autoCycle).toMatchObject({ started: true, runs: 0, failures: 0 });

    await timers.advance(1_000);
    expect(plugin!.scheduler!.status().runs).toBe(1);

    await fake.emit("dispose");
    expect(plugin!.scheduler!.status().started).toBe(false);
    expect(timers.pending()).toBe(0);
  });

  it("reports recipe failures instead of throwing", async () => {
    const store = createMemoryStore();
    await store.records.append(makeRecord({ recordId: "r1", response: "done" }));
    await store.feedback.append(makeFeedback({ recordId: "r1", score: 0.1 }));
    const fake = createFakeCtx();
    const plugin = apply(
      fake.ctx,
      { enabled: true, autoApply: true, recipes: ["skillclaw"], rubricEnabled: false, artifactRepo: dir },
      { store, runCommand: run, logger: () => undefined },
    );
    const report = await plugin!.cycle();
    expect(report.results[0]?.applied).toBe(false);
    expect(report.results[0]?.error).toMatch(/no LLM proposer|no bundle|failed/);
  });
});
