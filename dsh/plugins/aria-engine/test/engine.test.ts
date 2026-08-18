import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import {
  chatStream,
  ENGINE_SERVE_HINT,
  listModels,
  parseSseBody,
} from "../src/engine.ts";

function sseResponse(body: string, status = 200): Response {
  return new Response(body, { status, headers: { "content-type": "text/event-stream" } });
}

describe("parseSseBody", () => {
  it("emits text then finish; nothing after finish", () => {
    const body = [
      'data: {"choices":[{"delta":{"role":"assistant","content":"hi"},"finish_reason":null}]}',
      "",
      'data: {"choices":[{"delta":{},"finish_reason":"stop"}]}',
      "",
      "data: [DONE]",
      "",
    ].join("\n");
    const events = [...parseSseBody(body)];
    assert.deepEqual(
      events.map((e) => e.type),
      ["text", "finish"],
    );
    assert.equal(events[0].type === "text" ? events[0].text : "", "hi");
    assert.equal(events[1].type === "finish" ? events[1].reason : "", "stop");
  });

  it("emits usage before finish when usage is present", () => {
    const body = [
      'data: {"choices":[{"delta":{"content":"ok"},"finish_reason":null}]}',
      "",
      'data: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":3,"completion_tokens":1}}',
      "",
      "data: [DONE]",
      "",
    ].join("\n");
    const events = [...parseSseBody(body)];
    const types = events.map((e) => e.type);
    const finishAt = types.indexOf("finish");
    const usageAt = types.indexOf("usage");
    assert.ok(usageAt >= 0 && usageAt < finishAt);
    assert.equal(types.lastIndexOf("finish"), types.length - 1);
  });
});

describe("listModels / chatStream", () => {
  it("parses GET /v1/models", async () => {
    const fetchImpl: typeof fetch = async (input) => {
      assert.equal(String(input), "http://127.0.0.1:8080/v1/models");
      return Response.json({
        object: "list",
        data: [{ id: "gemma/tiny", object: "model", owned_by: "aria" }],
      });
    };
    const models = await listModels("http://127.0.0.1:8080/v1", { fetchImpl });
    assert.deepEqual(models, [{ id: "gemma/tiny", ownedBy: "aria" }]);
  });

  it("throws ENGINE_UNREACHABLE with serve hint", async () => {
    const fetchImpl: typeof fetch = async () => {
      throw new Error("ECONNREFUSED");
    };
    await assert.rejects(
      () => listModels("http://127.0.0.1:8080/v1", { fetchImpl }),
      (err: unknown) => {
        assert.ok(err instanceof AriaError);
        assert.equal(err.code, ErrorCode.ENGINE_UNREACHABLE);
        assert.match(err.message, new RegExp(ENGINE_SERVE_HINT.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")));
        return true;
      },
    );
  });

  it("streams chat SSE through fetch", async () => {
    const fetchImpl: typeof fetch = async (_input, init) => {
      assert.equal(JSON.parse(String(init?.body)).stream, true);
      return sseResponse(
        'data: {"choices":[{"delta":{"content":"hello"},"finish_reason":null}]}\n\ndata: [DONE]\n\n',
      );
    };
    const events = [];
    for await (const ev of chatStream({
      engineUrl: "http://127.0.0.1:8080/v1",
      messages: [{ role: "user", content: "hi" }],
      fetchImpl,
    })) {
      events.push(ev);
    }
    assert.equal(events[0]?.type, "text");
    assert.equal(events.at(-1)?.type, "finish");
  });

  it("maps HTTP errors", async () => {
    const fetchImpl: typeof fetch = async () => new Response("nope", { status: 502 });
    await assert.rejects(
      () =>
        chatStream({
          engineUrl: "http://127.0.0.1:8080/v1",
          messages: [{ role: "user", content: "hi" }],
          fetchImpl,
        }).next(),
      (err: unknown) => {
        assert.ok(err instanceof AriaError);
        assert.equal(err.code, ErrorCode.ENGINE_HTTP);
        return true;
      },
    );
  });

  it("honors AbortSignal", async () => {
    const signal = AbortSignal.abort();
    const fetchImpl: typeof fetch = async (_url, init) => {
      if (init?.signal?.aborted) {
        throw new DOMException("aborted", "AbortError");
      }
      return sseResponse("");
    };
    await assert.rejects(
      () =>
        chatStream({
          engineUrl: "http://127.0.0.1:8080/v1",
          messages: [{ role: "user", content: "hi" }],
          signal,
          fetchImpl,
        }).next(),
      (err: unknown) => {
        assert.ok(err instanceof AriaError);
        assert.equal(err.code, ErrorCode.ENGINE_UNREACHABLE);
        return true;
      },
    );
  });
});
