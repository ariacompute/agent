/** Fail-loud error for Aria harness plugins. */
export class AriaError extends Error {
  readonly code: string;

  constructor(message: string, code: string) {
    super(message);
    this.name = "AriaError";
    this.code = code;
  }
}

export const ErrorCode = {
  ENGINE_UNREACHABLE: "ENGINE_UNREACHABLE",
  ENGINE: "ENGINE",
  ENGINE_HTTP: "ENGINE_HTTP",
  MEMO_CLI: "MEMO_CLI",
  INVALID_PARAM: "INVALID_PARAM",
  EMPTY_CONTENT: "EMPTY_CONTENT",
  SANDBOX: "SANDBOX",
  SANDBOX_TIMEOUT: "SANDBOX_TIMEOUT",
  WORKSPACE: "WORKSPACE",
} as const;

export type ErrorCodeName = (typeof ErrorCode)[keyof typeof ErrorCode];
