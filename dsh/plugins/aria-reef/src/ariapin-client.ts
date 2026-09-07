import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { DEFAULT_ARIAPIN_TIMEOUT_MS, DEFAULT_ARIAPIN_URL } from "./config.ts";

export interface AriapinConfig {
  baseUrl: string;
  apiKey: string;
  timeoutMs: number;
}

export const DEFAULT_JOB_TYPE = "sft";
export const DEFAULT_DATASET_FORMAT = "sft";
export const TERMINAL_JOB_STATUSES = ["succeeded", "failed", "canceled", "cancelled", "stopped"];

export interface MinimalResponse {
  ok: boolean;
  status: number;
  text(): Promise<string>;
}

export type FetchLike = (
  url: string,
  init: { method: string; headers: Record<string, string>; body?: string | FormData; signal?: AbortSignal },
) => Promise<MinimalResponse>;

export interface JobStatus {
  id: string;
  status: string;
  progress?: Record<string, unknown>;
  modelId?: string;
  mlflowUrl?: string;
  raw: Record<string, unknown>;
}

export interface DatasetHandle {
  datasetId: string;
  raw: Record<string, unknown>;
}

export interface JobHandle {
  jobId: string;
  raw: Record<string, unknown>;
}

export interface AriapinClient {
  createDataset(input: {
    name: string;
    format?: string;
    content: string;
    filename?: string;
  }): Promise<DatasetHandle>;
  createJob(input: {
    baseModel: string;
    type?: string;
    datasetId?: string;
    teacherModel?: string;
    lora?: Record<string, unknown>;
    hyperparams?: Record<string, unknown>;
  }): Promise<JobHandle>;
  startJob(jobId: string): Promise<void>;
  getJob(jobId: string): Promise<JobStatus>;
  waitForJob(
    jobId: string,
    options?: { pollMs?: number; timeoutMs?: number; signal?: AbortSignal; sleep?: (ms: number) => Promise<void> },
  ): Promise<JobStatus>;
  /** Trigger a hot reload of the served weights (SGLang) or restart its agent. */
  reloadModel(input: { modelId?: string; agentId?: string }): Promise<{ reloaded: boolean; detail: string }>;
}

function truncate(value: string, max = 300): string {
  return value.length > max ? `${value.slice(0, max)}…` : value;
}

/** Unwrap the `{code, data, message}` envelope used by ariapin. */
function unwrap(payload: unknown, status: number, fallbackMessage: string): Record<string, unknown> {
  if (payload && typeof payload === "object" && !Array.isArray(payload)) {
    const envelope = payload as { code?: unknown; message?: unknown; data?: unknown };
    const code = typeof envelope.code === "number" ? envelope.code : 0;
    if (code !== 0) {
      throw new AriaError(
        `ariapin: ${typeof envelope.message === "string" && envelope.message ? envelope.message : fallbackMessage}`,
        ErrorCode.ARIAPIN,
      );
    }
    if (envelope.data && typeof envelope.data === "object" && !Array.isArray(envelope.data)) {
      return envelope.data as Record<string, unknown>;
    }
    if (envelope.data !== undefined && envelope.data !== null) {
      return { value: envelope.data };
    }
    return payload as Record<string, unknown>;
  }
  if (!Number.isNaN(status) && status >= 400) {
    throw new AriaError(`ariapin: ${fallbackMessage}`, ErrorCode.ARIAPIN);
  }
  return {};
}

function defaultSleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function createAriapinClient(
  config: Partial<AriapinConfig> = {},
  deps: { fetch?: FetchLike; sleep?: (ms: number) => Promise<void> } = {},
): AriapinClient {
  const baseUrl = (config.baseUrl ?? DEFAULT_ARIAPIN_URL).replace(/\/+$/, "");
  const apiKey = config.apiKey ?? "";
  const timeoutMs = config.timeoutMs ?? DEFAULT_ARIAPIN_TIMEOUT_MS;
  const doFetch = deps.fetch ?? (globalThis.fetch as unknown as FetchLike);
  const sleep = deps.sleep ?? defaultSleep;

  async function request(
    method: string,
    path: string,
    body?: string | FormData,
    json = true,
  ): Promise<Record<string, unknown>> {
    if (!doFetch) {
      throw new AriaError("ariapin: no fetch implementation available", ErrorCode.ARIAPIN);
    }
    const headers: Record<string, string> = { Authorization: `Bearer ${apiKey}` };
    if (json) {
      headers["Content-Type"] = "application/json";
    }
    const controller = new AbortController();
    const timer = setTimeout(() => controller.abort(), timeoutMs);
    let response: MinimalResponse;
    try {
      response = await doFetch(`${baseUrl}${path}`, {
        method,
        headers,
        ...(body !== undefined ? { body } : {}),
        signal: controller.signal,
      });
    } catch (err) {
      throw new AriaError(
        `ariapin: ${method} ${path} failed: ${err instanceof Error ? err.message : String(err)}`,
        ErrorCode.ARIAPIN,
      );
    } finally {
      clearTimeout(timer);
    }
    const text = await response.text();
    if (!response.ok) {
      throw new AriaError(
        `ariapin: ${method} ${path} -> HTTP ${response.status}: ${truncate(text)}`,
        ErrorCode.ARIAPIN,
      );
    }
    let parsed: unknown = undefined;
    if (text.trim().length > 0) {
      try {
        parsed = JSON.parse(text);
      } catch {
        parsed = { data: text };
      }
    }
    return unwrap(parsed, response.status, `${method} ${path} -> HTTP ${response.status}`);
  }

  return {
    async createDataset(input) {
      const form = new FormData();
      form.append("name", input.name);
      form.append("format", input.format ?? DEFAULT_DATASET_FORMAT);
      form.append(
        "file",
        new Blob([input.content], { type: "application/octet-stream" }),
        input.filename ?? `${input.name}.jsonl`,
      );
      const data = await request("POST", "/v1/datasets", form, false);
      const datasetId = typeof data.dataset_id === "string" ? data.dataset_id : "";
      if (!datasetId) {
        throw new AriaError(
          `ariapin: createDataset response has no dataset_id`,
          ErrorCode.ARIAPIN,
        );
      }
      return { datasetId, raw: data };
    },
    async createJob(input) {
      const body: Record<string, unknown> = {
        type: input.type ?? DEFAULT_JOB_TYPE,
        base_model: input.baseModel,
      };
      if (input.datasetId) {
        body.dataset_id = input.datasetId;
      }
      if (input.teacherModel) {
        body.teacher_model = input.teacherModel;
      }
      if (input.lora) {
        body.lora = input.lora;
      }
      if (input.hyperparams) {
        body.hyperparams = input.hyperparams;
      }
      const data = await request("POST", "/v1/jobs", JSON.stringify(body));
      const jobId = typeof data.job_id === "string" ? data.job_id : typeof data.id === "string" ? data.id : "";
      if (!jobId) {
        throw new AriaError("ariapin: createJob response has no job_id", ErrorCode.ARIAPIN);
      }
      return { jobId, raw: data };
    },
    async startJob(jobId) {
      await request("POST", `/v1/jobs/${encodeURIComponent(jobId)}/start`);
    },
    async getJob(jobId) {
      const data = await request("GET", `/v1/jobs/${encodeURIComponent(jobId)}`);
      const status = typeof data.status === "string" ? data.status : "unknown";
      const job: JobStatus = { id: jobId, status, raw: data };
      if (typeof data.model_id === "string") {
        job.modelId = data.model_id;
      }
      if (typeof data.mlflow_url === "string") {
        job.mlflowUrl = data.mlflow_url;
      }
      if (data.progress && typeof data.progress === "object" && !Array.isArray(data.progress)) {
        job.progress = data.progress as Record<string, unknown>;
      }
      return job;
    },
    async waitForJob(jobId, options = {}) {
      const pollMs = options.pollMs ?? 2000;
      const timeoutMs = options.timeoutMs ?? 600_000;
      const doSleep = options.sleep ?? sleep;
      const deadline = Date.now() + timeoutMs;
      let last: JobStatus = await this.getJob(jobId);
      while (!TERMINAL_JOB_STATUSES.includes(last.status)) {
        if (options.signal?.aborted) {
          throw new AriaError(`ariapin: wait for job ${jobId} aborted`, ErrorCode.REEF_TRAIN);
        }
        if (Date.now() >= deadline) {
          throw new AriaError(
            `ariapin: job ${jobId} still ${last.status} after ${timeoutMs}ms`,
            ErrorCode.REEF_TRAIN,
          );
        }
        await doSleep(pollMs);
        last = await this.getJob(jobId);
      }
      return last;
    },
    async reloadModel(input) {
      if (input.modelId) {
        try {
          const data = await request("POST", `/v1/models/${encodeURIComponent(input.modelId)}/reload`);
          return { reloaded: true, detail: `model ${input.modelId} reload requested: ${truncate(JSON.stringify(data))}` };
        } catch (err) {
          // Older/unmanaged deployments may not expose the reload endpoint: fall back
          // to restarting the agent that serves the weights when we know its id.
          if (!input.agentId) {
            throw err;
          }
          const reason = err instanceof Error ? err.message : String(err);
          await request("POST", `/v1/agents/${encodeURIComponent(input.agentId)}/restart`);
          return {
            reloaded: true,
            detail:
              `model ${input.modelId} reload unavailable (${truncate(reason, 160)}); ` +
              `fell back to restarting agent ${input.agentId}`,
          };
        }
      }
      if (input.agentId) {
        await request("POST", `/v1/agents/${encodeURIComponent(input.agentId)}/restart`);
        return { reloaded: true, detail: `agent ${input.agentId} restart requested` };
      }
      throw new AriaError(
        "ariapin: reloadModel requires modelId or agentId",
        ErrorCode.INVALID_PARAM,
      );
    },
  };
}

export interface FakeAriapinState {
  datasets: Array<{ name: string; content: string }>;
  jobs: Array<Record<string, unknown>>;
  started: string[];
  reloads: Array<{ modelId?: string; agentId?: string }>;
}

/**
 * Offline double: records every call and reports jobs as `succeeded` by default.
 * `jobStatus` may be a string or a per-call sequence.
 */
export function createFakeAriapinClient(
  options: {
    jobStatus?: string | (() => string);
    modelId?: string;
    failOn?: "dataset" | "job" | "reload";
  } = {},
): { client: AriapinClient; state: FakeAriapinState } {
  const state: FakeAriapinState = { datasets: [], jobs: [], started: [], reloads: [] };
  let counter = 0;
  const resolveStatus = (): string => {
    if (typeof options.jobStatus === "function") {
      return options.jobStatus();
    }
    return options.jobStatus ?? "succeeded";
  };

  const client: AriapinClient = {
    async createDataset(input) {
      if (options.failOn === "dataset") {
        throw new AriaError("ariapin: fake dataset failure", ErrorCode.ARIAPIN);
      }
      state.datasets.push({ name: input.name, content: input.content });
      counter += 1;
      return { datasetId: `ds_${counter}`, raw: { dataset_id: `ds_${counter}` } };
    },
    async createJob(input) {
      if (options.failOn === "job") {
        throw new AriaError("ariapin: fake job failure", ErrorCode.ARIAPIN);
      }
      counter += 1;
      const jobId = `jb_${counter}`;
      state.jobs.push({ ...input });
      return { jobId, raw: { job_id: jobId } };
    },
    async startJob(jobId) {
      state.started.push(jobId);
    },
    async getJob(jobId) {
      return {
        id: jobId,
        status: resolveStatus(),
        modelId: options.modelId,
        raw: { job_id: jobId, status: resolveStatus() },
      };
    },
    async waitForJob(jobId) {
      return client.getJob(jobId);
    },
    async reloadModel(input) {
      if (options.failOn === "reload") {
        throw new AriaError("ariapin: fake reload failure", ErrorCode.ARIAPIN);
      }
      state.reloads.push({ modelId: input.modelId, agentId: input.agentId });
      return { reloaded: true, detail: `fake reload ${input.modelId ?? input.agentId}` };
    },
  };
  return { client, state };
}
