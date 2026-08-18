import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  DEFAULT_E2B_API_KEY,
  DEFAULT_E2B_API_URL,
  DEFAULT_TIMEOUT_MS,
  loadSandboxConfig,
  withOverrides,
} from "../src/config.ts";

describe("loadSandboxConfig", () => {
  it("uses defaults", () => {
    const cfg = loadSandboxConfig({});
    assert.equal(cfg.apiKey, DEFAULT_E2B_API_KEY);
    assert.equal(cfg.apiUrl, DEFAULT_E2B_API_URL);
    assert.equal(cfg.template, undefined);
    assert.equal(cfg.timeoutMs, DEFAULT_TIMEOUT_MS);
    assert.equal(cfg.workspaceRoot, join(homedir(), ".ariacompute", "agent", "workspaces"));
    assert.equal(cfg.syncAfterExec, false);
    assert.equal(cfg.workspaceId, undefined);
  });

  it("reads env overrides", () => {
    const cfg = loadSandboxConfig({
      E2B_API_KEY: "e2b_x",
      E2B_API_URL: "http://cube:3000",
      CUBE_TEMPLATE_ID: "tpl-1",
      E2B_TIMEOUT_MS: "120000",
      ARIA_WORKSPACE_ROOT: "/tmp/ws",
      ARIA_WORKSPACE_SYNC_AFTER_EXEC: "1",
      ARIA_WORKSPACE_ID: "ws-a",
    });
    assert.equal(cfg.apiKey, "e2b_x");
    assert.equal(cfg.apiUrl, "http://cube:3000");
    assert.equal(cfg.template, "tpl-1");
    assert.equal(cfg.timeoutMs, 120000);
    assert.equal(cfg.workspaceRoot, "/tmp/ws");
    assert.equal(cfg.syncAfterExec, true);
    assert.equal(cfg.workspaceId, "ws-a");
  });

  it("falls back on bad timeout", () => {
    const cfg = loadSandboxConfig({ E2B_TIMEOUT_MS: "abc" });
    assert.equal(cfg.timeoutMs, DEFAULT_TIMEOUT_MS);
  });

  it("withOverrides merges partials", () => {
    const base = loadSandboxConfig({});
    const merged = withOverrides(base, { apiKey: "k2", syncAfterExec: true });
    assert.equal(merged.apiKey, "k2");
    assert.equal(merged.syncAfterExec, true);
    assert.equal(merged.apiUrl, DEFAULT_E2B_API_URL);
    assert.equal(merged.timeoutMs, DEFAULT_TIMEOUT_MS);
  });
});
