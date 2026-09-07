import { describe, it, expect } from "bun:test";
import type { Sandbox } from "e2b";
import { e2bSandboxFactory, toSandboxClient } from "../src/e2b-client.ts";

/**
 * e2b's `FileType.DIR` value; kept as a literal so the test does not depend on
 * the SDK's runtime module graph.
 */
const FILE_TYPE_DIR = "dir";

interface WriteCall {
  path: string;
  data: unknown;
}

interface FakeSandboxOptions {
  /** Rejects instead of resolving; message is used as the error text. */
  writeError?: string;
  readError?: string;
  readBytes?: Uint8Array;
  entries?: Array<{ name: string; path: string; type?: string }>;
}

interface FakeSandbox {
  sandbox: Sandbox;
  writes: WriteCall[];
  reads: Array<{ path: string; options: unknown }>;
  dirs: string[];
  killed: number;
  runs: Array<{ command: string; options: unknown }>;
}

/** e2b-shaped fake; injected into the adapter so tests stay fully offline. */
function fakeSandbox(opts: FakeSandboxOptions = {}): FakeSandbox {
  const state: FakeSandbox = {
    sandbox: undefined as unknown as Sandbox,
    writes: [],
    reads: [],
    dirs: [],
    killed: 0,
    runs: [],
  };

  state.sandbox = {
    async kill() {
      state.killed += 1;
    },
    commands: {
      async run(command: string, options: unknown) {
        state.runs.push({ command, options });
        return { stdout: `out:${command}`, stderr: "", exitCode: 0 };
      },
    },
    files: {
      async write(path: string, data: unknown) {
        if (opts.writeError) {
          throw new Error(opts.writeError);
        }
        state.writes.push({ path, data });
        return { path };
      },
      async read(path: string, options: unknown) {
        if (opts.readError) {
          throw new Error(opts.readError);
        }
        state.reads.push({ path, options });
        return opts.readBytes ?? new Uint8Array([1, 2, 3]);
      },
      async list(path: string) {
        return (opts.entries ?? []).map((entry) => ({ ...entry, path: entry.path || path }));
      },
      async makeDir(path: string) {
        state.dirs.push(path);
      },
    },
  } as unknown as Sandbox;

  return state;
}

async function blobBytes(value: unknown): Promise<Uint8Array> {
  expect(value instanceof Blob).toBe(true);
  return new Uint8Array(await (value as Blob).arrayBuffer());
}

describe("e2b adapter files.write", () => {
  it("converts a string payload into a UTF-8 Blob", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);

    await client.files.write("/workspace/a.txt", "hello 世界");

    expect(fake.writes.length).toBe(1);
    expect(fake.writes[0].path).toBe("/workspace/a.txt");
    const bytes = await blobBytes(fake.writes[0].data);
    expect(new TextDecoder().decode(bytes)).toBe("hello 世界");
  });

  it("converts a Uint8Array payload into a byte-identical Blob", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);
    const payload = new Uint8Array([0, 1, 254, 255, 7]);

    await client.files.write("/workspace/b.bin", payload);

    const bytes = await blobBytes(fake.writes[0].data);
    expect([...bytes]).toEqual([...payload]);
  });

  it("respects Buffer views (byteOffset/length) when building the Blob", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);
    const buffer = Buffer.from([9, 8, 7, 6, 5]);
    const view = buffer.subarray(1, 4); // [8, 7, 6]

    await client.files.write("/workspace/c.bin", view);

    const bytes = await blobBytes(fake.writes[0].data);
    expect([...bytes]).toEqual([8, 7, 6]);
  });

  it("writes an empty payload without failing", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);

    await client.files.write("/workspace/empty.txt", "");

    const bytes = await blobBytes(fake.writes[0].data);
    expect(bytes.byteLength).toBe(0);
  });

  it("propagates SDK write failures instead of swallowing them", async () => {
    const fake = fakeSandbox({ writeError: "upload aborted" });
    const client = toSandboxClient(fake.sandbox);

    let err: unknown;
    try {
      await client.files.write("/workspace/a.txt", "x");
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(Error);
    expect((err as Error).message).toBe("upload aborted");
  });
});

describe("e2b adapter passthrough", () => {
  it("reads with the bytes format and returns raw bytes", async () => {
    const fake = fakeSandbox({ readBytes: new Uint8Array([4, 5]) });
    const client = toSandboxClient(fake.sandbox);

    const data = await client.files.read("/workspace/a.txt");

    expect(fake.reads).toEqual([{ path: "/workspace/a.txt", options: { format: "bytes" } }]);
    expect([...data]).toEqual([4, 5]);
  });

  it("propagates read failures", async () => {
    const fake = fakeSandbox({ readError: "not found" });
    const client = toSandboxClient(fake.sandbox);

    let err: unknown;
    try {
      await client.files.read("/workspace/missing.txt");
    } catch (e) {
      err = e;
    }
    expect((err as Error).message).toBe("not found");
  });

  it("maps list entries to SandboxFileInfo (dirs flagged by type)", async () => {
    const fake = fakeSandbox({
      entries: [
        { name: "a.txt", path: "/workspace/a.txt", type: "file" },
        { name: "src", path: "/workspace/src", type: FILE_TYPE_DIR },
      ],
    });
    const client = toSandboxClient(fake.sandbox);

    const entries = await client.files.list("/workspace");

    expect(entries).toEqual([
      { name: "a.txt", path: "/workspace/a.txt", isDir: false },
      { name: "src", path: "/workspace/src", isDir: true },
    ]);
  });

  it("forwards makeDir and kill to the SDK", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);

    await client.files.makeDir("/workspace/src");
    await client.kill();

    expect(fake.dirs).toEqual(["/workspace/src"]);
    expect(fake.killed).toBe(1);
  });

  it("maps command options onto the SDK run call", async () => {
    const fake = fakeSandbox();
    const client = toSandboxClient(fake.sandbox);

    const result = await client.commands.run("ls -l", {
      cwd: "/workspace",
      env: { A: "1" },
      timeout: 1500,
    });

    expect(fake.runs).toEqual([
      { command: "ls -l", options: { cwd: "/workspace", envs: { A: "1" }, timeoutMs: 1500 } },
    ]);
    expect(result).toEqual({ stdout: "out:ls -l", stderr: "", exitCode: 0 });
  });
});

describe("e2bSandboxFactory", () => {
  it("exposes the real SDK-backed factory", () => {
    expect(typeof e2bSandboxFactory.create).toBe("function");
  });
});
