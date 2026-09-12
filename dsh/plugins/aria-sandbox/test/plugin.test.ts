import { afterAll, describe, it, expect } from "bun:test";
import { rm } from "node:fs/promises";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { SandboxClient, SandboxFactory, WorkspaceRegistryLike } from "../src/types.ts";
import { apply, e2bSandboxFactory } from "../src/index.ts";
import { SANDBOX_WORKSPACE_DIR } from "../src/workspace.ts";

interface RegisteredTool {
  name: string;
  description: string;
  parameters: Record<string, unknown>;
  execute: (args: unknown, exec: unknown) => Promise<unknown>;
}

function makeCtx() {
  const tools: RegisteredTool[] = [];
  const listeners = new Map<string, Array<(...args: unknown[]) => unknown>>();
  return {
    tools: {
      register(def: unknown) {
        tools.push(def as RegisteredTool);
      },
    },
    llm: { registerAdapter() {} },
    on(event: string, listener: (...args: unknown[]) => unknown) {
      const list = listeners.get(event) ?? [];
      list.push(listener);
      listeners.set(event, list);
    },
    fire(event: string, ...args: unknown[]) {
      for (const fn of listeners.get(event) ?? []) {
        void fn(...args);
      }
    },
    toolsList: tools,
  };
}

type Ctx = ReturnType<typeof makeCtx>;

interface FakeCounts {
  created: number;
  killed: number;
}

/** Fake factory with a tiny in-memory blob store; counters are mutable. */
function fakeFactory(blobs?: Map<string, string>): { factory: SandboxFactory; counts: FakeCounts } {
  const counts: FakeCounts = { created: 0, killed: 0 };
  const factory: SandboxFactory = {
    async create() {
      counts.created += 1;
      return {
        async kill() {
          counts.killed += 1;
        },
        commands: {
          async run(command) {
            return { stdout: `out:${command}`, stderr: "", exitCode: 0 };
          },
        },
        files: {
          async write(path, data) {
            const buf = typeof data === "string" ? Buffer.from(data, "utf8") : Buffer.from(data);
            blobs?.set(path, buf.toString("utf8"));
          },
          async read(path) {
            const value = blobs?.get(path);
            if (value === undefined) {
              throw new Error(`not found: ${path}`);
            }
            return Buffer.from(value);
          },
          async list(path) {
            if (!blobs) {
              return [];
            }
            const prefix = path === "/" ? "/" : `${path}/`;
            const children = new Map<string, { name: string; isDir: boolean }>();
            for (const key of blobs.keys()) {
              if (!key.startsWith(prefix)) {
                continue;
              }
              const rest = key.slice(prefix.length);
              const top = rest.split("/")[0];
              if (top && !children.has(top)) {
                children.set(top, { name: top, isDir: false });
              }
            }
            return [...children.entries()].map(([name]) => ({
              name,
              path: posix.join(path, name),
              isDir: false,
            }));
          },
          async makeDir() {},
        },
      } satisfies SandboxClient;
    },
  };
  return { factory, counts };
}

function buildCtx(opts: {
  workspaceRoot: string;
  registry?: WorkspaceRegistryLike;
  factory?: SandboxFactory;
  config?: Record<string, unknown>;
}) {
  const ctx = makeCtx();
  apply(ctx, opts.config ?? {}, {
    factory: opts.factory,
    registry: opts.registry,
    workspaceRoot: opts.workspaceRoot,
  });
  return ctx;
}

function tool(ctx: Ctx, name: string): RegisteredTool {
  const found = ctx.toolsList.find((t) => t.name === name);
  expect(found, `tool ${name} registered`).toBeDefined();
  return found!;
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

describe("aria-sandbox plugin", () => {
  let root = "";
  afterAll(async () => {
    if (root) {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("registers all seven tools", () => {
    root = makeTmpRoot();
    const ctx = buildCtx({ workspaceRoot: root });
    expect(ctx.toolsList.map((t) => t.name)).toEqual([
      "sandbox_exec",
      "sandbox_read_file",
      "sandbox_write_file",
      "sandbox_list_files",
      "sandbox_sync_to_host",
      "sandbox_sync_from_host",
      "workspace_status",
    ]);
  });

  it("exec runs inside the sandbox and keys workspace by sessionId", async () => {
    root = makeTmpRoot();
    const { factory, counts } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory });
    const result = await tool(ctx, "sandbox_exec").execute(
      { command: "ls" },
      { sessionId: "sess-1" },
    );
    expect(result).toEqual({ stdout: "out:ls", stderr: "", exitCode: 0 });
    expect(counts.created).toBe(1);
  });

  it("same workspace reuses the sandbox; different ids get separate ones", async () => {
    root = makeTmpRoot();
    const { factory, counts } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory });
    await tool(ctx, "sandbox_exec").execute({ command: "a" }, { sessionId: "s1" });
    await tool(ctx, "sandbox_exec").execute({ command: "b" }, { sessionId: "s1" });
    await tool(ctx, "sandbox_exec").execute({ command: "c" }, { sessionId: "s2" });
    expect(counts.created).toBe(2);
  });

  it("explicit workspace arg overrides session id", async () => {
    root = makeTmpRoot();
    const { factory, counts } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory });
    await tool(ctx, "sandbox_exec").execute(
      { command: "x", workspace: "ws-explicit" },
      { sessionId: "s1" },
    );
    await tool(ctx, "sandbox_exec").execute({ command: "y" }, { sessionId: "s1" });
    expect(counts.created).toBe(2);
  });

  it("syncAfterExec pulls files back after exec (cubesandbox)", async () => {
    root = makeTmpRoot();
    const blobs = new Map<string, string>();
    const { factory } = fakeFactory(blobs);
    const ctx = buildCtx({
      workspaceRoot: root,
      factory,
      config: { sandboxType: "cubesandbox", syncAfterExec: true },
    });
    await tool(ctx, "sandbox_write_file").execute(
      { path: `${SANDBOX_WORKSPACE_DIR}/f.txt`, content: "hi" },
      { sessionId: "s1" },
    );
    await tool(ctx, "sandbox_exec").execute({ command: "touch" }, { sessionId: "s1" });
    // f.txt was written to sandbox; sync after exec must persist it to host
    const status = await tool(ctx, "workspace_status").execute({}, { sessionId: "s1" });
    expect((status as { files: number }).files).toBe(1);
  });

  it("rejects sandbox paths outside /workspace", async () => {
    root = makeTmpRoot();
    const { factory } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory });
    let err: unknown;
    try {
      await tool(ctx, "sandbox_read_file").execute(
        { path: "/etc/passwd" },
        { sessionId: "s1" },
      );
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.INVALID_PARAM);
  });

  it("wraps sandbox create failures as SANDBOX", async () => {
    root = makeTmpRoot();
    const failing: SandboxFactory = {
      async create() {
        throw new Error("KVM unavailable");
      },
    };
    const ctx = buildCtx({ workspaceRoot: root, factory: failing });
    let err: unknown;
    try {
      await tool(ctx, "sandbox_exec").execute({ command: "x" }, { sessionId: "s1" });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.SANDBOX);
    expect((err as AriaError).message).toMatch(/KVM unavailable/);
  });

  it("registers workspace with dsh registry once per workspace", async () => {
    root = makeTmpRoot();
    const created: string[] = [];
    const registry: WorkspaceRegistryLike = {
      get: () => [],
      create(path, title) {
        created.push(`${path}:${title}`);
      },
    };
    const { factory } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, registry, factory });
    await tool(ctx, "sandbox_exec").execute({ command: "a" }, { sessionId: "s1" });
    await tool(ctx, "sandbox_exec").execute({ command: "b" }, { sessionId: "s1" });
    expect(created.length).toBe(1);
    expect(created[0]).toMatch(/s1/);
  });

  it("dispose kills sandboxes", async () => {
    root = makeTmpRoot();
    const { factory, counts } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory });
    await tool(ctx, "sandbox_exec").execute({ command: "a" }, { sessionId: "s1" });
    await tool(ctx, "sandbox_exec").execute({ command: "b" }, { sessionId: "s2" });
    expect(counts.killed).toBe(0);
    ctx.fire("dispose");
    await sleep(30);
    expect(counts.killed).toBe(2);
  });

  it("exports the real e2b factory", () => {
    expect(typeof e2bSandboxFactory.create).toBe("function");
  });
});

describe("aria-sandbox backend selection", () => {
  let root = "";
  afterAll(async () => {
    if (root) {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("builds a real container factory for the default docker backend without breaking registration", () => {
    root = makeTmpRoot();
    const ctx = buildCtx({ workspaceRoot: root });
    expect(ctx.toolsList.length).toBe(7);
  });

  it("fails loudly on an invalid ARIA_SANDBOX_TYPE", () => {
    root = makeTmpRoot();
    let err: unknown;
    try {
      buildCtx({
        workspaceRoot: root,
        config: { sandboxType: "podman" as unknown as string },
      });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.INVALID_PARAM);
  });

  it("sync tools are no-ops for container backends", async () => {
    root = makeTmpRoot();
    const { factory } = fakeFactory();
    const ctx = buildCtx({ workspaceRoot: root, factory, config: { sandboxType: "docker" } });
    const toHost = await tool(ctx, "sandbox_sync_to_host").execute({}, { sessionId: "s1" });
    const fromHost = await tool(ctx, "sandbox_sync_from_host").execute({}, { sessionId: "s1" });
    expect(toHost).toEqual({ synced: 0 });
    expect(fromHost).toEqual({ synced: 0 });
  });

  it("cubesandbox backend still routes sync through the client", async () => {
    root = makeTmpRoot();
    const blobs = new Map<string, string>();
    const { factory } = fakeFactory(blobs);
    const ctx = buildCtx({ workspaceRoot: root, factory, config: { sandboxType: "cubesandbox" } });
    await tool(ctx, "sandbox_write_file").execute(
      { path: `${SANDBOX_WORKSPACE_DIR}/f.txt`, content: "hi" },
      { sessionId: "s1" },
    );
    const res = await tool(ctx, "sandbox_sync_to_host").execute({}, { sessionId: "s1" });
    expect((res as { synced: number }).synced).toBe(1);
  });
});

function makeTmpRoot(): string {
  return mkdtempSync(join(tmpdir(), "aria-sandbox-plugin-"));
}
