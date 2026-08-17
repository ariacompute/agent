import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { AriaError, ErrorCode } from "../src/error.ts";
import {
  memoAdd,
  memoForget,
  memoGet,
  memoList,
  memoSearch,
} from "../src/memo.ts";
import type { RunCommand } from "../src/spawn.ts";
import { loadConfig } from "../src/config.ts";

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
          assert.ok(args.includes("--db"));
          assert.ok(args.includes("add"));
          return { code: 0, stdout: "m123\n" };
        }),
      },
    );
    assert.equal(id, "m123");
  });

  it("rejects empty content", async () => {
    await assert.rejects(
      () => memoAdd({ content: "  " }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "x" })) }),
      (err: unknown) => {
        assert.ok(err instanceof AriaError);
        assert.equal(err.code, ErrorCode.EMPTY_CONTENT);
        return true;
      },
    );
  });

  it("parses search lines", async () => {
    const hits = await memoSearch(
      { text: "rust", topK: 5 },
      {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "0.900\tuser likes rust\n0.100\tother\n" })),
      },
    );
    assert.equal(hits.length, 2);
    assert.equal(hits[0].score, 0.9);
    assert.equal(hits[0].content, "user likes rust");
  });

  it("get returns null for not found", async () => {
    const rec = await memoGet("missing", {
      config: cfg,
      runCommand: script(() => ({ code: 0, stdout: "not found\n" })),
    });
    assert.equal(rec, null);
  });

  it("get parses JSON", async () => {
    const rec = await memoGet("m1", {
      config: cfg,
      runCommand: script(() => ({ code: 0, stdout: '{"id":"m1","content":"hi"}\n' })),
    });
    assert.equal(rec?.id, "m1");
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
    assert.equal(rows.length, 2);
    assert.equal(rows[0].id, "m1");
    assert.equal(rows[1].content, "world");
  });

  it("forget found vs missing", async () => {
    assert.equal(
      await memoForget("m1", {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "forgotten\n" })),
      }),
      true,
    );
    assert.equal(
      await memoForget("m2", {
        config: cfg,
        runCommand: script(() => ({ code: 0, stdout: "not found\n" })),
      }),
      false,
    );
  });

  it("non-zero exit is MEMO_CLI", async () => {
    await assert.rejects(
      () =>
        memoSearch(
          { text: "x" },
          { config: cfg, runCommand: script(() => ({ code: 1, stdout: "", stderr: "error: boom" })) },
        ),
      (err: unknown) => {
        assert.ok(err instanceof AriaError);
        assert.equal(err.code, ErrorCode.MEMO_CLI);
        assert.match(err.message, /boom/);
        return true;
      },
    );
  });

  it("rejects invalid type and topK", async () => {
    await assert.rejects(() => memoAdd({ content: "x", type: "nope" }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "id" })) }));
    await assert.rejects(() => memoSearch({ text: "x", topK: 0 }, { config: cfg, runCommand: script(() => ({ code: 0, stdout: "" })) }));
  });
});
