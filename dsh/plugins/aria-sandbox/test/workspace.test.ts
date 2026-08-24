import { afterAll, describe, it, expect } from "bun:test";
import { mkdtemp, mkdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, posix } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import type { SandboxClient, WorkspaceRegistryLike } from "../src/types.ts";
import {
  assertSandboxPath,
  collectHostFiles,
  ensureHostWorkspace,
  normalizeWorkspaceId,
  registerWorkspace,
  SANDBOX_WORKSPACE_DIR,
  syncFromHost,
  syncToHost,
  workspaceStatus,
} from "../src/workspace.ts";

/** In-memory fake e2b client for offline tests. */
function fakeSandbox(initial?: Record<string, string>): SandboxClient {
  const dirs = new Set<string>(["/", "/workspace"]);
  const blobs = new Map<string, string>();
  for (const [path, content] of Object.entries(initial ?? {})) {
    blobs.set(path, content);
  }
  return {
    async kill() {},
    commands: {
      async run(command) {
        return { stdout: `ran:${command}`, stderr: "", exitCode: 0 };
      },
    },
    files: {
      async write(path, data) {
        const buf = typeof data === "string" ? Buffer.from(data, "utf8") : Buffer.from(data);
        blobs.set(path, buf.toString("utf8"));
      },
      async read(path) {
        const value = blobs.get(path);
        if (value === undefined) {
          throw new Error(`not found: ${path}`);
        }
        return Buffer.from(value);
      },
      async list(path) {
        const prefix = path === "/" ? "/" : `${path}/`;
        // every intermediate segment of a blob path is a directory
        const dirSet = new Set<string>(dirs);
        for (const key of blobs.keys()) {
          let p = posix.dirname(key);
          while (p && p !== "/" && p !== ".") {
            dirSet.add(p);
            p = posix.dirname(p);
          }
        }
        const children = new Map<string, boolean>(); // name -> isDir
        for (const key of blobs.keys()) {
          if (!key.startsWith(prefix)) {
            continue;
          }
          const rest = key.slice(prefix.length);
          const top = rest.split("/")[0];
          if (top && !children.has(top)) {
            children.set(top, false);
          }
        }
        for (const dir of dirSet) {
          if (dir === path || !dir.startsWith(prefix)) {
            continue;
          }
          const rest = dir.slice(prefix.length);
          const top = rest.split("/")[0];
          if (top) {
            children.set(top, true);
          }
        }
        return [...children.entries()].map(([name, isDir]) => ({
          name,
          path: posix.join(path, name),
          isDir,
        }));
      },
      async makeDir(path) {
        dirs.add(path);
      },
    },
  };
}

describe("normalizeWorkspaceId", () => {
  it("keeps safe ids and sanitizes others", () => {
    expect(normalizeWorkspaceId("ws-a_1.x")).toBe("ws-a_1.x");
    expect(normalizeWorkspaceId("a b/c")).toBe("a-b-c");
    expect(normalizeWorkspaceId("..")).toBe("default");
    expect(normalizeWorkspaceId("")).toBe("default");
    expect(normalizeWorkspaceId(undefined)).toBe("default");
  });
});

describe("assertSandboxPath", () => {
  it("accepts paths under /workspace", () => {
    expect(assertSandboxPath("/workspace/a.txt")).toBe("/workspace/a.txt");
    expect(assertSandboxPath("/workspace")).toBe("/workspace");
  });

  it("rejects escapes and relatives", () => {
    for (const bad of ["/etc/passwd", "/workspace/../x", "a/b", "/", "/workspace-other/x"]) {
      expect(() => assertSandboxPath(bad)).toThrowError(AriaError);
      try {
        assertSandboxPath(bad);
      } catch (err) {
        expect((err as AriaError).code).toBe(ErrorCode.INVALID_PARAM);
      }
    }
  });
});

describe("host workspace isolation", () => {
  let root = "";
  afterAll(async () => {
    if (root) {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("creates one dir per workspace id and keeps them separate", async () => {
    root = await mkdtemp(join(tmpdir(), "aria-ws-"));
    const dirA = await ensureHostWorkspace(root, "agent-A");
    const dirB = await ensureHostWorkspace(root, "agent-B");
    expect(dirA).not.toBe(dirB);
    await writeFile(join(dirA, "secret.txt"), "a");
    const filesA = (await collectHostFiles(dirA)).map((f) => f.rel);
    const filesB = (await collectHostFiles(dirB)).map((f) => f.rel);
    expect(filesA).toEqual(["secret.txt"]);
    expect(filesB).toEqual([]);
  });

  it("collects nested files and reports status", async () => {
    await mkdir(join(root, "agent-C", "sub"), { recursive: true });
    await writeFile(join(root, "agent-C", "sub", "f.txt"), "hello");
    const status = await workspaceStatus(join(root, "agent-C"), "agent-C");
    expect(status.files).toBe(1);
    expect(status.totalBytes).toBe(5);
  });
});

describe("syncFromHost / syncToHost", () => {
  let root = "";
  afterAll(async () => {
    if (root) {
      await rm(root, { recursive: true, force: true });
    }
  });

  it("round-trips host -> sandbox -> host", async () => {
    root = await mkdtemp(join(tmpdir(), "aria-sync-"));
    const hostDir = await ensureHostWorkspace(root, "agent-X");
    await mkdir(join(hostDir, "src"), { recursive: true });
    await writeFile(join(hostDir, "README.md"), "# hi");
    await writeFile(join(hostDir, "src", "main.rs"), "fn main() {}");

    const sandbox = fakeSandbox();
    const pushed = await syncFromHost(sandbox, hostDir);
    expect(pushed).toBe(2);

    const pulled = await syncToHost(sandbox, hostDir);
    expect(pulled).toBe(2);
    expect(await readFile(join(hostDir, "README.md"), "utf8")).toBe("# hi");
    expect(await readFile(join(hostDir, "src", "main.rs"), "utf8")).toBe("fn main() {}");
  });

  it("syncToHost rejects entries that escape /workspace", async () => {
    root = await mkdtemp(join(tmpdir(), "aria-sync-"));
    const hostDir = await ensureHostWorkspace(root, "agent-Y");
    const evil: SandboxClient = {
      async kill() {},
      commands: { async run() { return { stdout: "", stderr: "", exitCode: 0 }; } },
      files: {
        async write() {},
        async read() {
          return Buffer.from("x");
        },
        async list() {
          return [{ name: "..", path: "/workspace/../etc/passwd", isDir: false }];
        },
        async makeDir() {},
      },
    };
    let err: unknown;
    try {
      await syncToHost(evil, hostDir);
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.WORKSPACE);
  });
});

describe("registerWorkspace", () => {
  it("no-ops without registry", async () => {
    await registerWorkspace(undefined, "/tmp/x", "a");
  });

  it("calls registry.create and tolerates errors", async () => {
    let created: string | undefined;
    const registry: WorkspaceRegistryLike = {
      get: () => [],
      create(path, title) {
        created = `${path}:${title}`;
      },
    };
    await registerWorkspace(registry, "/tmp/x", "a");
    expect(created).toBe("/tmp/x:a");

    const broken: WorkspaceRegistryLike = {
      get: () => [],
      create() {
        throw new Error("already registered");
      },
    };
    await registerWorkspace(broken, "/tmp/x", "a"); // must not throw
  });
});

describe("sandbox dir constants", () => {
  it("workspace dir is /workspace", () => {
    expect(SANDBOX_WORKSPACE_DIR).toBe("/workspace");
  });
});
