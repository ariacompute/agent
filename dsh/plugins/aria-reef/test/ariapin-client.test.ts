import { describe, expect, it } from "bun:test";
import { ErrorCode } from "../../shared/src/error.ts";
import {
  createAriapinClient,
  type FetchLike,
  type MinimalResponse,
} from "../src/ariapin-client.ts";

interface Call {
  url: string;
  init: { method: string; headers: Record<string, string>; body?: string | FormData };
}

function json(status: number, body: unknown): MinimalResponse {
  return { ok: status >= 200 && status < 300, status, text: async () => JSON.stringify(body) };
}

function collector(
  handler: (url: string, init: Call["init"]) => MinimalResponse | Promise<MinimalResponse>,
): { calls: Call[]; fetch: FetchLike } {
  const calls: Call[] = [];
  const fetch: FetchLike = async (url, init) => {
    calls.push({ url, init });
    return handler(url, init);
  };
  return { calls, fetch };
}

const okEnvelope = (data: Record<string, unknown>) => json(200, { code: 0, data });

describe("ariapin client", () => {
  it("uploads datasets as multipart with bearer auth", async () => {
    const { calls, fetch } = collector(() => okEnvelope({ dataset_id: "ds_1" }));
    const client = createAriapinClient(
      { baseUrl: "http://ariapin:8001/", apiKey: "sk-1", timeoutMs: 1000 },
      { fetch },
    );
    const result = await client.createDataset({ name: "reef-sft", content: "{}" });
    expect(result.datasetId).toBe("ds_1");
    expect(calls[0]?.url).toBe("http://ariapin:8001/v1/datasets");
    expect(calls[0]?.init.headers.Authorization).toBe("Bearer sk-1");
    const body = calls[0]?.init.body as FormData;
    expect(body.get("name")).toBe("reef-sft");
    expect(body.get("format")).toBe("sft");
    expect(await (body.get("file") as Blob).text()).toBe("{}");
  });

  it("fails loudly when the dataset id is missing", async () => {
    const { fetch } = collector(() => okEnvelope({}));
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    await expect(client.createDataset({ name: "x", content: "" })).rejects.toThrow(/no dataset_id/);
  });

  it("creates jobs with the REST contract keys", async () => {
    const { calls, fetch } = collector(() => okEnvelope({ job_id: "jb_1" }));
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    const result = await client.createJob({
      baseModel: "base/model",
      type: "grpo",
      datasetId: "ds_1",
      lora: { method: "lora", rank: 16 },
      hyperparams: { epochs: 1 },
    });
    expect(result.jobId).toBe("jb_1");
    const body = JSON.parse(calls[0]?.init.body as string);
    expect(body).toEqual({
      type: "grpo",
      base_model: "base/model",
      dataset_id: "ds_1",
      lora: { method: "lora", rank: 16 },
      hyperparams: { epochs: 1 },
    });
  });

  it("fails loudly when the job id is missing", async () => {
    const { fetch } = collector(() => okEnvelope({}));
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    await expect(client.createJob({ baseModel: "m" })).rejects.toThrow(/no job_id/);
  });

  it("starts jobs and maps job status", async () => {
    const { calls, fetch } = collector(() =>
      okEnvelope({ status: "running", model_id: "mdl_9", progress: { step: 3 } }),
    );
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    await client.startJob("jb_1");
    expect(calls[0]?.url).toContain("/v1/jobs/jb_1/start");
    const job = await client.getJob("jb_1");
    expect(job.status).toBe("running");
    expect(job.modelId).toBe("mdl_9");
    expect(job.progress).toEqual({ step: 3 });
  });

  it("polls until the job reaches a terminal state", async () => {
    let polls = 0;
    const { fetch } = collector(() => {
      polls += 1;
      return okEnvelope({ status: polls >= 3 ? "succeeded" : "running" });
    });
    const client = createAriapinClient({ apiKey: "k" }, { fetch, sleep: async () => undefined });
    const job = await client.waitForJob("jb_1", { pollMs: 1, timeoutMs: 5000 });
    expect(job.status).toBe("succeeded");
    expect(polls).toBe(3);
  });

  it("times out instead of hanging forever", async () => {
    const { fetch } = collector(() => okEnvelope({ status: "running" }));
    const client = createAriapinClient({ apiKey: "k" }, { fetch, sleep: async () => undefined });
    await expect(client.waitForJob("jb_1", { pollMs: 1, timeoutMs: 0 })).rejects.toThrow(/still running/);
  });

  it("maps HTTP, envelope and transport failures to AriaError", async () => {
    const http = createAriapinClient({ apiKey: "k" }, { fetch: collector(() => json(500, "boom")).fetch });
    await expect(http.getJob("jb_1")).rejects.toThrow(/HTTP 500/);

    const envelope = createAriapinClient(
      { apiKey: "k" },
      { fetch: collector(() => json(200, { code: 7, message: "quota exceeded" })).fetch },
    );
    await expect(envelope.getJob("jb_1")).rejects.toThrow(/quota exceeded/);
    try {
      await envelope.getJob("jb_1");
    } catch (err) {
      expect((err as { code?: string }).code).toBe(ErrorCode.ARIAPIN);
    }

    const transport = createAriapinClient(
      { apiKey: "k" },
      {
        fetch: (() => {
          throw new Error("socket hang up");
        }) as unknown as FetchLike,
      },
    );
    await expect(transport.getJob("jb_1")).rejects.toThrow(/socket hang up/);
  });

  it("reloads weights via model or agent endpoints", async () => {
    const { calls, fetch } = collector(() => okEnvelope({}));
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    expect((await client.reloadModel({ modelId: "mdl_1" })).reloaded).toBe(true);
    expect(calls[0]?.url).toContain("/v1/models/mdl_1/reload");
    await client.reloadModel({ agentId: "agt_1" });
    expect(calls[1]?.url).toContain("/v1/agents/agt_1/restart");
    await expect(client.reloadModel({})).rejects.toThrow(/requires modelId or agentId/);
  });

  it("falls back to restarting the agent when the reload endpoint is missing", async () => {
    const { calls, fetch } = collector((url) =>
      url.includes("/models/") ? json(404, { code: 404, message: "not found" }) : okEnvelope({}),
    );
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    const result = await client.reloadModel({ modelId: "mdl_1", agentId: "agt_1" });
    expect(result.reloaded).toBe(true);
    expect(result.detail).toContain("fell back to restarting agent agt_1");
    expect(calls.map((call) => call.url)).toEqual([
      expect.stringContaining("/v1/models/mdl_1/reload"),
      expect.stringContaining("/v1/agents/agt_1/restart"),
    ]);
  });

  it("rethrows the reload failure when no fallback agent is known", async () => {
    const { calls, fetch } = collector(() => json(404, { code: 404, message: "not found" }));
    const client = createAriapinClient({ apiKey: "k" }, { fetch });
    await expect(client.reloadModel({ modelId: "mdl_1" })).rejects.toThrow(/HTTP 404/);
    expect(calls.length).toBe(1);
  });
});
