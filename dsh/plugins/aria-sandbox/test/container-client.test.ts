import { afterAll, describe, it, expect } from "bun:test";
import { mkdtempSync } from "node:fs";
import { rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { ContainerExecutor } from "../src/types.ts";
import { createContainerSandboxFactory } from "../src/container-client.ts";

interface FakeExecutorOpts {
  cid?: string;
  createExitCode?: number;
  createError?: string;
  execExitCode?: number;
}

/** Offline executor that records every CLI invocation and returns canned data. */
function fakeExecutor(opts: FakeExecutorOpts = {}) {
  const calls: string[][] = [];
  const executor: ContainerExecutor = {
    async run(args: string[]) {
      calls.push(args);
      if (args.includes("create")) {
        if (opts.createError) {
          throw new Error(opts.createError);
        }
        return {
          stdout: opts.cid ?? "cid-123",
          stderr: opts.createExitCode ? "boom" : "",
          exitCode: opts.createExitCode ?? 0,
        };
      }
      if (args.includes("rm")) {
        return { stdout: "", stderr: "", exitCode: 0 };
      }
      if (args.includes("exec")) {
        const command = args[args.length - 1];
        return {
          stdout: `out:${command}`,
          stderr: "",
          exitCode: opts.execExitCode ?? 0,
        };
      }
      return { stdout: "", stderr: "", exitCode: 0 };
    },
  };
  return { executor, calls };
}

describe("createContainerSandboxFactory — create", () => {
  it("mounts the host dir and uses the kata runtime when provided", async () => {
    const { executor, calls } = fakeExecutor();
    const hostDir = join(tmpdir(), "aria-ct-kata");
    const factory = createContainerSandboxFactory({
      executor,
      image: "img:1",
      runtime: "kata",
    });
    const client = await factory.create({ hostDir });

    expect(calls[0][0]).toBe("create");
    expect(calls[0]).toContain("--runtime");
    expect(calls[0]).toContain("kata");
    expect(calls[0]).toContain(`${hostDir}:/workspace`);
    expect(calls[0]).toContain("-w");
    expect(calls[0]).toContain("img:1");
    expect(typeof client.kill).toBe("function");
  });

  it("omits the runtime flag for plain docker", async () => {
    const { executor, calls } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    await factory.create({ hostDir: join(tmpdir(), "aria-ct-docker") });

    expect(calls[0]).not.toContain("--runtime");
  });

  it("requires a hostDir", async () => {
    const { executor } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor });
    let err: unknown;
    try {
      await factory.create({});
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.SANDBOX);
  });

  it("wraps a failed create as SANDBOX", async () => {
    const { executor } = fakeExecutor({ createExitCode: 1 });
    const factory = createContainerSandboxFactory({ executor });
    let err: unknown;
    try {
      await factory.create({ hostDir: join(tmpdir(), "aria-ct-fail") });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.SANDBOX);
  });

  it("propagates a process-level create error", async () => {
    const { executor } = fakeExecutor({ createError: "cli missing" });
    const factory = createContainerSandboxFactory({ executor });
    let err: unknown;
    try {
      await factory.create({ hostDir: join(tmpdir(), "aria-ct-err") });
    } catch (e) {
      err = e;
    }
    expect((err as Error).message).toMatch(/cli missing/);
  });
});

describe("createContainerSandboxFactory — commands.run", () => {
  it("runs via exec and returns stdout/stderr/exitCode", async () => {
    const { executor } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    const client = await factory.create({ hostDir: join(tmpdir(), "aria-ct-run") });

    const result = await client.commands.run("echo hi", { cwd: "/workspace/sub", timeout: 2000 });

    expect(result).toEqual({ stdout: "out:echo hi", stderr: "", exitCode: 0 });
  });

  it("wraps the exec call in a timeout when a timeout is set", async () => {
    const { executor, calls } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    const client = await factory.create({ hostDir: join(tmpdir(), "aria-ct-timeout") });

    await client.commands.run("sleep 9", { timeout: 1500 });

    const execCall = calls.find((c) => c.includes("exec"))!;
    expect(execCall.slice(0, 2)).toEqual(["timeout", "2s"]);
  });
});

describe("createContainerSandboxFactory — files (host FS)", () => {
  let hostDir = "";
  afterAll(async () => {
    if (hostDir) {
      await rm(hostDir, { recursive: true, force: true });
    }
  });

  it("write/read/list/makeDir operate on the mounted host dir", async () => {
    hostDir = mkdtempSync(join(tmpdir(), "aria-ct-fs-"));
    const { executor } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    const client = await factory.create({ hostDir });

    await client.files.write("/workspace/a.txt", "hello 世界");
    await client.files.makeDir("/workspace/dir");

    const data = await client.files.read("/workspace/a.txt");
    expect(new TextDecoder().decode(data)).toBe("hello 世界");

    const listing = await client.files.list("/workspace");
    const names = listing.map((e) => e.name).sort();
    expect(names).toEqual(["a.txt", "dir"]);
  });

  it("rejects sandbox paths that escape /workspace", async () => {
    hostDir = mkdtempSync(join(tmpdir(), "aria-ct-esc-"));
    const { executor } = fakeExecutor();
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    const client = await factory.create({ hostDir });

    let err: unknown;
    try {
      await client.files.read("/etc/passwd");
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.INVALID_PARAM);
  });
});

describe("createContainerSandboxFactory — kill", () => {
  it("issues rm -f with the container id", async () => {
    const { executor, calls } = fakeExecutor({ cid: "cid-abc" });
    const factory = createContainerSandboxFactory({ executor, image: "img:1" });
    const client = await factory.create({ hostDir: join(tmpdir(), "aria-ct-kill") });

    await client.kill();

    const rmCall = calls.find((c) => c[0] === "rm");
    expect(rmCall).toEqual(["rm", "-f", "cid-abc"]);
  });
});
