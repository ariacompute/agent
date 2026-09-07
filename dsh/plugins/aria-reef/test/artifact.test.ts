import { afterEach, beforeEach, describe, expect, it } from "bun:test";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { ErrorCode } from "../../shared/src/error.ts";
import { assertArtifactName, createArtifactRepo } from "../src/artifact.ts";
import { LFS_PATTERNS } from "../src/config.ts";
import type { RunCommand } from "../../shared/src/index.ts";

let dir: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "reef-artifact-"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

interface GitStub {
  calls: string[][];
  run: RunCommand;
}

function gitStub(
  handler: (subcommand: string | undefined, args: string[]) => { code?: number; stdout?: string; stderr?: string } = () => ({}),
): GitStub {
  const calls: string[][] = [];
  const run: RunCommand = async (command, args) => {
    calls.push([command, ...args]);
    // git is always invoked as: git -C <root> <subcommand> ...
    return { code: 0, stdout: "", stderr: "", ...handler(args[2], [...args]) };
  };
  return { calls, run };
}

describe("assertArtifactName", () => {
  it("accepts sane paths and rejects traversal", () => {
    expect(assertArtifactName("skills/agent")).toBe("skills/agent");
    expect(assertArtifactName("/skills/agent")).toBe("skills/agent");
    expect(() => assertArtifactName("../etc/passwd")).toThrow(/invalid artifact name/);
    expect(() => assertArtifactName("  ")).toThrow(/must not be empty/);
  });
});

describe("artifact repo", () => {
  it("init writes LFS attributes and bootstraps git", async () => {
    const { calls, run } = gitStub();
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    await repo.init();
    const attributes = readFileSync(join(dir, ".gitattributes"), "utf8");
    for (const pattern of LFS_PATTERNS) {
      expect(attributes).toContain(pattern);
    }
    expect(attributes).toContain("filter=lfs");
    expect(calls.some((call) => call.join(" ").includes("git -C") && call.includes("init"))).toBe(true);
    expect(calls.some((call) => call.includes("user.email"))).toBe(true);
    expect(calls.some((call) => call.includes("lfs"))).toBe(true);
  });

  it("tolerates a missing git-lfs binary", async () => {
    const logged: string[] = [];
    const { run } = gitStub((sub) => (sub === "lfs" ? { code: 127, stderr: "not found" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run, logger: (m) => logged.push(m) });
    await repo.init();
    expect(logged.some((line) => line.includes("git-lfs unavailable"))).toBe(true);
  });

  it("versions writes and keeps history snapshots", async () => {
    const { run } = gitStub((sub) => (sub === "rev-parse" ? { stdout: "abc123\n" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    const ref = { kind: "skill" as const, name: "skills/agent" };
    expect(await repo.read(ref)).toBeNull();
    expect(await repo.activeVersion(ref)).toBe(0);

    const first = await repo.write(ref, "v1 content");
    expect(first.version).toBe(1);
    expect(await repo.read(ref)).toBe("v1 content");
    expect(await repo.activeVersion(ref)).toBe(1);
    expect(await repo.readVersion(ref, 1)).toBe("v1 content");

    const second = await repo.write(ref, "v2 content");
    expect(second.version).toBe(2);
    expect(await repo.read(ref)).toBe("v2 content");
    expect(await repo.readVersion(ref, 1)).toBe("v1 content");
    expect(await repo.readVersion(ref, 2)).toBe("v2 content");
    expect(await repo.readVersion(ref, 9)).toBeNull();
  });

  it("rejects unsafe artifact names on write", async () => {
    const { run } = gitStub();
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    await expect(repo.write({ kind: "skill", name: "../escape" }, "x")).rejects.toThrow(
      /invalid artifact name/,
    );
  });

  it("commits and returns the sha", async () => {
    const { calls, run } = gitStub((sub) => (sub === "rev-parse" ? { stdout: "deadbeef\n" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    await repo.write({ kind: "prompt", name: "prompts/system" }, "hello");
    const sha = await repo.commit("reef: update");
    expect(sha).toBe("deadbeef");
    expect(calls.some((call) => call.includes("add") && call.includes("-A"))).toBe(true);
    expect(calls.some((call) => call.includes("commit"))).toBe(true);
  });

  it("returns null when there is nothing to commit", async () => {
    const logged: string[] = [];
    const { run } = gitStub((sub) => (sub === "commit" ? { code: 1, stderr: "nothing to commit" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run, logger: (m) => logged.push(m) });
    expect(await repo.commit("noop")).toBeNull();
    expect(logged.some((line) => line.includes("nothing to commit"))).toBe(true);
  });

  it("fails loudly when git itself fails", async () => {
    const { run } = gitStub((sub) => (sub === "add" ? { code: 128, stderr: "boom" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    await expect(repo.commit("x")).rejects.toThrow(/git add -A failed/);
    try {
      await repo.commit("x");
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.REEF_GIT);
    }
  });

  it("parses git log output", async () => {
    const { run } = gitStub((sub) => (sub === "log" ? { stdout: "abc first\ndef second\n" } : {}));
    const repo = createArtifactRepo({ root: dir, runCommand: run });
    expect(await repo.log(5)).toEqual(["abc first", "def second"]);
  });
});
