import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { RunCommand } from "../../shared/src/index.ts";
import { createArtifactRepo } from "../src/artifact.ts";
import { createFakeAriapinClient } from "../src/ariapin-client.ts";
import { createSurface } from "../src/surface.ts";

let dir: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "reef-surface-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

const run: RunCommand = async (_command, args) => ({
  code: 0,
  stdout: args[2] === "rev-parse" ? "sha1\n" : "",
  stderr: "",
});

describe("surface", () => {
  it("reports nothing before the first publish", async () => {
    const surface = createSurface({ artifact: createArtifactRepo({ root: dir, runCommand: run }) });
    expect(await surface.active({ kind: "skill", name: "skills/agent" })).toBeNull();
  });

  it("serves freshly published content without a restart", async () => {
    const artifact = createArtifactRepo({ root: dir, runCommand: run });
    const surface = createSurface({ artifact });
    const ref = { kind: "skill" as const, name: "skills/agent" };

    const first = await surface.publish(ref, "v1");
    expect(first.version).toBe(1);
    expect(first.commit).toBe("sha1");
    expect(await surface.active(ref)).toEqual({ version: 1, content: "v1" });

    const second = await surface.publish(ref, "v2 hot-patched");
    expect(second.version).toBe(2);
    const active = await surface.active(ref);
    expect(active?.content).toBe("v2 hot-patched");
    expect(active?.version).toBe(2);
  });

  it("reloads weights through ariapin when configured", async () => {
    const { client, state } = createFakeAriapinClient();
    const artifact = createArtifactRepo({ root: dir, runCommand: run });
    const surface = createSurface({ artifact, ariapin: client });
    const result = await surface.reloadWeight({ modelId: "mdl_1" });
    expect(result.reloaded).toBe(true);
    expect(state.reloads).toEqual([{ modelId: "mdl_1", agentId: undefined }]);
  });

  it("degrades gracefully without an ariapin client", async () => {
    const logged: string[] = [];
    const artifact = createArtifactRepo({ root: dir, runCommand: run });
    const surface = createSurface({ artifact, logger: (m) => logged.push(m) });
    const result = await surface.reloadWeight({ modelId: "mdl_1" });
    expect(result.reloaded).toBe(false);
    expect(logged.some((line) => line.includes("no ariapin client"))).toBe(true);
  });
});
