import { spawn } from "node:child_process";
import { AriaError, ErrorCode } from "./error.ts";

export interface CommandResult {
  code: number;
  stdout: string;
  stderr: string;
}

export type RunCommand = (
  command: string,
  args: readonly string[],
  options?: { signal?: AbortSignal },
) => Promise<CommandResult>;

export const defaultRunCommand: RunCommand = (command, args, options) => {
  return new Promise((resolve, reject) => {
    const child = spawn(command, [...args], {
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    child.stdout.setEncoding("utf8");
    child.stderr.setEncoding("utf8");
    child.stdout.on("data", (chunk: string) => {
      stdout += chunk;
    });
    child.stderr.on("data", (chunk: string) => {
      stderr += chunk;
    });

    const onAbort = (): void => {
      child.kill("SIGTERM");
    };
    if (options?.signal) {
      if (options.signal.aborted) {
        child.kill("SIGTERM");
      } else {
        options.signal.addEventListener("abort", onAbort, { once: true });
      }
    }

    child.on("error", (err: NodeJS.ErrnoException) => {
      options?.signal?.removeEventListener("abort", onAbort);
      if (err.code === "ENOENT") {
        reject(
          new AriaError(
            `memo CLI not found: ${command}. Build with cargo build -p aria-memo --release and set ARIA_MEMO_BIN.`,
            ErrorCode.MEMO_CLI,
          ),
        );
        return;
      }
      reject(new AriaError(err.message, ErrorCode.MEMO_CLI));
    });

    child.on("close", (code) => {
      options?.signal?.removeEventListener("abort", onAbort);
      resolve({
        code: code ?? 1,
        stdout,
        stderr,
      });
    });
  });
};
