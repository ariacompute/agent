import { describe, it, expect } from "bun:test";
import { homedir } from "node:os";
import { join } from "node:path";
import { defaultMemoDb, loadConfig, normalizeEngineUrl } from "../src/config.ts";

describe("normalizeEngineUrl", () => {
  it("appends /v1 when missing", () => {
    expect(normalizeEngineUrl("http://127.0.0.1:8080")).toBe("http://127.0.0.1:8080/v1");
  });

  it("strips trailing slashes and keeps /v1", () => {
    expect(normalizeEngineUrl("http://127.0.0.1:8080/v1/")).toBe("http://127.0.0.1:8080/v1");
  });
});

describe("loadConfig", () => {
  it("uses defaults", () => {
    const cfg = loadConfig({});
    expect(cfg.engineUrl).toBe("http://127.0.0.1:8080/v1");
    expect(cfg.memoBin).toBe("aria-memo");
    expect(cfg.memoDb).toBe(join(homedir(), ".ariacompute", "memo.db"));
    expect(defaultMemoDb()).toBe(cfg.memoDb);
  });

  it("reads env overrides", () => {
    const cfg = loadConfig({
      ARIA_ENGINE_URL: "http://localhost:9",
      ARIA_MEMO_BIN: "/opt/aria-memo",
      ARIA_MEMO_DB: "/tmp/m.db",
    });
    expect(cfg.engineUrl).toBe("http://localhost:9/v1");
    expect(cfg.memoBin).toBe("/opt/aria-memo");
    expect(cfg.memoDb).toBe("/tmp/m.db");
  });
});
