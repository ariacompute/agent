import { describe, it, expect } from "bun:test";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { loadConfig } from "../../shared/src/config.ts";
import type { RunCommand } from "../../shared/src/spawn.ts";
import {
  memoAdd,
  memoForget,
  memoGet,
  memoList,
  memoSearch,
} from "../src/memo.ts";

function script(handler: (args: readonly string[]) => { code: number; stdout: string; stderr?: string }): RunCommand {
  return async (_cmd, args) => {
    const result = handler(args);
    return { code: result.code, stdout: result.stdout, stderr: result.stderr ?? "" };
  };
}

const cfg = loadConfig({ ARIA_MEMO_BIN: "aria-memo", ARIA_MEMO_DB: "/tmp/t.db" });

describe("memo CLI wrapper", () => {
  it("add returns id", async () => {
    const id = await memoAdd(
      { content: "user likes rust", type: "working", importance: 0.8 },
      {
        config: cfg,
        runCommand: script((args) => {
          expect(args.includes("--db")).toBe(true);
          expect(args.includes("add")).toBe(true);
          return { code: 0, stdout: "m123\n" };
        }),
      },
    );
    expect(id).toBe("m123");
  });

  it("rejects empty content", async () => {
    let err: unknown;
    try {
      await memoAdd({ content: "  " }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "x" })) });
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.EMPTY_CONTENT);
  });

  it("parses search lines", async () => {
    const hits = await memoSearch(
      { text: "rust", topK: 5 },
      {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "0.900\tuser likes rust\n0.100\tother\n" })),
      },
    );
    expect(hits.length).toBe(2);
    expect(hits[0].score).toBe(0.9);
    expect(hits[0].content).toBe("user likes rust");
  });

  it("get returns null for not found", async () => {
    const rec = await memoGet("missing", {
      config: cfg,
      runCommand: script(() => ({ code: 0, stdout: "not found\n" })),
    });
    expect(rec).toBe(null);
  });

  it("get parses JSON", async () => {
    const rec = await memoGet("m1", {
      config: cfg,
      runCommand: script(() => ({ code: 0, stdout: '{"id":"m1","content":"hi"}\n' })),
    });
    expect(rec?.id).toBe("m1");
  });

  it("list parses debug lines", async () => {
    const rows = await memoList(
      {},
      {
        config: cfg,
        runCommand: script(() => ({
          code: 0,
          stdout: "m1 [Working] hello\nm2 [LongTerm { kind: Episodic }] world\n",
        })),
      },
    );
    expect(rows.length).toBe(2);
    expect(rows[0].id).toBe("m1");
    expect(rows[1].content).toBe("world");
  });

  it("forget found vs missing", async () => {
    expect(
      await memoForget("m1", {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "forgotten\n" })),
      }),
    ).toBe(true);
    expect(
      await memoForget("m2", {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "not found\n" })),
      }),
    ).toBe(false);
  });

  it("non-zero exit is MEMO_CLI", async () => {
    let err: unknown;
    try {
      await memoSearch(
        { text: "x" },
        { config: cfg, runCommand: script(() => ({ code: 1, stdout: "", stderr: "error: boom" })) },
      );
    } catch (e) {
      err = e;
    }
    expect(err).toBeInstanceOf(AriaError);
    expect((err as AriaError).code).toBe(ErrorCode.MEMO_CLI);
    expect((err as AriaError).message).toMatch(/boom/);
  });

  it("rejects invalid type and topK", async () => {
    await expect(
      memoAdd({ content: "x", type: "nope" }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "id" })) }),
    ).rejects.toThrow();
    await expect(
      memoSearch({ text: "x", topK: 0 }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "" })) }),
    ).rejects.toThrow();
  });
});
