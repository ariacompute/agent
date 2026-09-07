import { describe, it, expect } from "bun:test";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  DEFAULT_ARIAPIN_URL,
  DEFAULT_BATCH_SIZE,
  DEFAULT_CYCLE_BACKOFF_FACTOR,
  DEFAULT_CYCLE_INTERVAL_MS,
  DEFAULT_CYCLE_MIN_SIGNALS,
  DEFAULT_CYCLE_POLL_MS,
  DEFAULT_MIN_FEEDBACK,
  DEFAULT_RELEASE,
  defaultArtifactDir,
  defaultStoreDir,
  loadReefConfig,
  withOverrides,
} from "../src/config.ts";

describe("loadReefConfig", () => {
  it("defaults to disabled and local dirs", () => {
    const cfg = loadReefConfig({});
    expect(cfg.enabled).toBe(false);
    expect(cfg.release).toBe(DEFAULT_RELEASE);
    expect(cfg.storeDir).toBe(join(homedir(), ".ariacompute", "agent", "reef"));
    expect(cfg.artifactRepo).toBe(defaultArtifactDir());
    expect(cfg.storeDir).toBe(defaultStoreDir());
    expect(cfg.batchSize).toBe(DEFAULT_BATCH_SIZE);
    expect(cfg.minFeedback).toBe(DEFAULT_MIN_FEEDBACK);
    expect(cfg.rubricEnabled).toBe(true);
    expect(cfg.autoApply).toBe(false);
    expect(cfg.recipes).toEqual(["skillclaw", "prompt", "rules"]);
    expect(cfg.ariapinUrl).toBe(DEFAULT_ARIAPIN_URL);
    expect(cfg.ariapinKey).toBe("");
    expect(cfg.bundle).toBeUndefined();
    expect(cfg.autoCycle).toBe(false);
    expect(cfg.cycleIntervalMs).toBe(DEFAULT_CYCLE_INTERVAL_MS);
    expect(cfg.cycleMinSignals).toBe(DEFAULT_CYCLE_MIN_SIGNALS);
    expect(cfg.cyclePollMs).toBe(DEFAULT_CYCLE_POLL_MS);
    expect(cfg.cycleMaxBackoffMs).toBe(DEFAULT_CYCLE_INTERVAL_MS * DEFAULT_CYCLE_BACKOFF_FACTOR);
  });

  it("reads ARIA_REEF_* overrides", () => {
    const cfg = loadReefConfig({
      ARIA_REEF_ENABLED: "on",
      ARIA_REEF_RELEASE: "v3",
      ARIA_REEF_STORE_DIR: "/tmp/reef-store",
      ARIA_REEF_ARTIFACT_REPO: "/tmp/reef-artifacts",
      ARIA_REEF_BATCH_SIZE: "4",
      ARIA_REEF_MIN_FEEDBACK: "2",
      ARIA_REEF_RUBRIC: "off",
      ARIA_REEF_AUTO_APPLY: "yes",
      ARIA_REEF_RECIPES: "prompt, weight",
      ARIA_REEF_ARIAPIN_URL: "http://ariapin:8001/",
      ARIA_REEF_ARIAPIN_KEY: "sk-test",
      ARIA_REEF_ARIAPIN_TIMEOUT_MS: "5000",
      ARIA_REEF_BUNDLE: "/models/bundle",
      ARIA_REEF_FFI_LIB: "/usr/lib/libaria_ffi.so",
      ARIA_REEF_MODEL: "aria-tiny",
      ARIA_REEF_BASE_MODEL: "base/model",
      ARIA_REEF_CYCLE: "on",
      ARIA_REEF_CYCLE_INTERVAL_MS: "600000",
      ARIA_REEF_CYCLE_MIN_SIGNALS: "3",
      ARIA_REEF_CYCLE_POLL_MS: "5000",
      ARIA_REEF_CYCLE_MAX_BACKOFF_MS: "120000",
    });
    expect(cfg.enabled).toBe(true);
    expect(cfg.release).toBe("v3");
    expect(cfg.storeDir).toBe("/tmp/reef-store");
    expect(cfg.artifactRepo).toBe("/tmp/reef-artifacts");
    expect(cfg.batchSize).toBe(4);
    expect(cfg.minFeedback).toBe(2);
    expect(cfg.rubricEnabled).toBe(false);
    expect(cfg.autoApply).toBe(true);
    expect(cfg.recipes).toEqual(["prompt", "weight"]);
    expect(cfg.ariapinUrl).toBe("http://ariapin:8001");
    expect(cfg.ariapinKey).toBe("sk-test");
    expect(cfg.ariapinTimeoutMs).toBe(5000);
    expect(cfg.bundle).toBe("/models/bundle");
    expect(cfg.ffiLib).toBe("/usr/lib/libaria_ffi.so");
    expect(cfg.model).toBe("aria-tiny");
    expect(cfg.baseModel).toBe("base/model");
    expect(cfg.autoCycle).toBe(true);
    expect(cfg.cycleIntervalMs).toBe(600_000);
    expect(cfg.cycleMinSignals).toBe(3);
    expect(cfg.cyclePollMs).toBe(5000);
    expect(cfg.cycleMaxBackoffMs).toBe(120_000);
  });

  it("derives cycle poll and backoff from the interval", () => {
    const cfg = loadReefConfig({ ARIA_REEF_CYCLE_INTERVAL_MS: "30000" });
    expect(cfg.cycleIntervalMs).toBe(30_000);
    expect(cfg.cyclePollMs).toBe(30_000);
    expect(cfg.cycleMaxBackoffMs).toBe(120_000);
  });

  it("accepts zero signals as a disabled threshold and rejects junk", () => {
    expect(loadReefConfig({ ARIA_REEF_CYCLE_MIN_SIGNALS: "0" }).cycleMinSignals).toBe(0);
    expect(loadReefConfig({ ARIA_REEF_CYCLE_MIN_SIGNALS: "x" }).cycleMinSignals).toBe(
      DEFAULT_CYCLE_MIN_SIGNALS,
    );
  });

  it("falls back on invalid numbers and booleans", () => {
    const cfg = loadReefConfig({
      ARIA_REEF_ENABLED: "maybe",
      ARIA_REEF_BATCH_SIZE: "0",
      ARIA_REEF_MIN_FEEDBACK: "abc",
      ARIA_REEF_RECIPES: " , ",
      ARIA_REEF_ARIAPIN_TIMEOUT_MS: "-1",
    });
    expect(cfg.enabled).toBe(false);
    expect(cfg.batchSize).toBe(DEFAULT_BATCH_SIZE);
    expect(cfg.minFeedback).toBe(DEFAULT_MIN_FEEDBACK);
    expect(cfg.recipes).toEqual(["skillclaw", "prompt", "rules"]);
  });

  it("withOverrides merges plugin config over env", () => {
    const base = loadReefConfig({});
    const merged = withOverrides(base, { enabled: true, recipes: ["skillclaw"], storeDir: "/tmp/x" });
    expect(merged.enabled).toBe(true);
    expect(merged.recipes).toEqual(["skillclaw"]);
    expect(merged.storeDir).toBe("/tmp/x");
    expect(merged.artifactRepo).toBe(base.artifactRepo);
    expect(merged.batchSize).toBe(DEFAULT_BATCH_SIZE);
  });
});
