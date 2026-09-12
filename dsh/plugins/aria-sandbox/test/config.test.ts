import { describe, it, expect } from "bun:test";
import { homedir } from "node:os";
import { join } from "node:path";
import {
  DEFAULT_E2B_API_KEY,
  DEFAULT_E2B_API_URL,
  DEFAULT_SANDBOX_CLI,
  DEFAULT_SANDBOX_IMAGE,
  DEFAULT_SANDBOX_TYPE,
  DEFAULT_TIMEOUT_MS,
  KATA_RUNTIME,
  loadSandboxConfig,
  withOverrides,
} from "../src/config.ts";

describe("loadSandboxConfig", () => {
  it("uses defaults (docker backend)", () => {
    const cfg = loadSandboxConfig({});
    expect(cfg.sandboxType).toBe(DEFAULT_SANDBOX_TYPE);
    expect(cfg.sandboxType).toBe("docker");
    expect(cfg.apiKey).toBe(DEFAULT_E2B_API_KEY);
    expect(cfg.apiUrl).toBe(DEFAULT_E2B_API_URL);
    expect(cfg.template).toBeUndefined();
    expect(cfg.timeoutMs).toBe(DEFAULT_TIMEOUT_MS);
    expect(cfg.containerImage).toBe(DEFAULT_SANDBOX_IMAGE);
    expect(cfg.containerCli).toBe(DEFAULT_SANDBOX_CLI);
    expect(cfg.containerRuntime).toBeUndefined();
    expect(cfg.workspaceRoot).toBe(join(homedir(), ".ariacompute", "agent", "workspaces"));
    expect(cfg.syncAfterExec).toBe(false);
    expect(cfg.workspaceId).toBeUndefined();
  });

  it("reads env overrides", () => {
    const cfg = loadSandboxConfig({
      E2B_API_KEY: "e2b_x",
      E2B_API_URL: "http://cube:3000",
      CUBE_TEMPLATE_ID: "tpl-1",
      E2B_TIMEOUT_MS: "120000",
      ARIA_SANDBOX_TYPE: "cubesandbox",
      ARIA_SANDBOX_IMAGE: "myimg:1.0",
      ARIA_SANDBOX_CLI: "nerdctl",
      ARIA_SANDBOX_RUNTIME: "io.containerd.kata.v2",
      ARIA_WORKSPACE_ROOT: "/tmp/ws",
      ARIA_WORKSPACE_SYNC_AFTER_EXEC: "1",
      ARIA_WORKSPACE_ID: "ws-a",
    });
    expect(cfg.sandboxType).toBe("cubesandbox");
    expect(cfg.apiKey).toBe("e2b_x");
    expect(cfg.apiUrl).toBe("http://cube:3000");
    expect(cfg.template).toBe("tpl-1");
    expect(cfg.timeoutMs).toBe(120000);
    expect(cfg.containerImage).toBe("myimg:1.0");
    expect(cfg.containerCli).toBe("nerdctl");
    expect(cfg.containerRuntime).toBe("io.containerd.kata.v2");
    expect(cfg.workspaceRoot).toBe("/tmp/ws");
    expect(cfg.syncAfterExec).toBe(true);
    expect(cfg.workspaceId).toBe("ws-a");
  });

  it("derives the kata runtime automatically for the kata backend", () => {
    const cfg = loadSandboxConfig({ ARIA_SANDBOX_TYPE: "kata" });
    expect(cfg.sandboxType).toBe("kata");
    expect(cfg.containerRuntime).toBe(KATA_RUNTIME);
  });

  it("does not force a runtime for docker unless explicitly set", () => {
    const cfg = loadSandboxConfig({ ARIA_SANDBOX_TYPE: "docker" });
    expect(cfg.containerRuntime).toBeUndefined();
  });

  it("falls back on bad timeout", () => {
    const cfg = loadSandboxConfig({ E2B_TIMEOUT_MS: "abc" });
    expect(cfg.timeoutMs).toBe(DEFAULT_TIMEOUT_MS);
  });

  it("rejects an unknown sandbox type", () => {
    expect(() => loadSandboxConfig({ ARIA_SANDBOX_TYPE: "podman" })).toThrow();
  });

  it("withOverrides merges partials", () => {
    const base = loadSandboxConfig({});
    const merged = withOverrides(base, {
      apiKey: "k2",
      syncAfterExec: true,
      sandboxType: "kata",
    });
    expect(merged.apiKey).toBe("k2");
    expect(merged.syncAfterExec).toBe(true);
    expect(merged.apiUrl).toBe(DEFAULT_E2B_API_URL);
    expect(merged.timeoutMs).toBe(DEFAULT_TIMEOUT_MS);
    expect(merged.sandboxType).toBe("kata");
  });
});
