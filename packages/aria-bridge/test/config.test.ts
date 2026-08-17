import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { homedir } from "node:os";
import { join } from "node:path";
import { defaultMemoDb, loadConfig, normalizeEngineUrl } from "../src/config.ts";

describe("normalizeEngineUrl", () => {
  it("appends /v1 when missing", () => {
    assert.equal(normalizeEngineUrl("http://127.0.0.1:8080"), "http://127.0.0.1:8080/v1");
  });

  it("strips trailing slashes and keeps /v1", () => {
    assert.equal(normalizeEngineUrl("http://127.0.0.1:8080/v1/"), "http://127.0.0.1:8080/v1");
  });
});

describe("loadConfig", () => {
  it("uses defaults", () => {
    const cfg = loadConfig({});
    assert.equal(cfg.engineUrl, "http://127.0.0.1:8080/v1");
    assert.equal(cfg.memoBin, "aria-memo");
    assert.equal(cfg.memoDb, join(homedir(), ".ariacompute", "memo.db"));
    assert.equal(defaultMemoDb(), cfg.memoDb);
  });

  it("reads env overrides", () => {
    const cfg = loadConfig({
      ARIA_ENGINE_URL: "http://localhost:9",
      ARIA_MEMO_BIN: "/opt/aria-memo",
      ARIA_MEMO_DB: "/tmp/m.db",
    });
    assert.equal(cfg.engineUrl, "http://localhost:9/v1");
    assert.equal(cfg.memoBin, "/opt/aria-memo");
    assert.equal(cfg.memoDb, "/tmp/m.db");
  });
});
