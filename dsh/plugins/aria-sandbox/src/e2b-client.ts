import { Sandbox, FileType } from "e2b";
import type {
  SandboxClient,
  SandboxCommandResult,
  SandboxFactory,
  SandboxFileInfo,
} from "./types.ts";

function toFileInfo(info: { name: string; path: string; type?: unknown }): SandboxFileInfo {
  return {
    name: info.name,
    path: info.path,
    isDir: info.type === FileType.DIR,
  };
}

/**
 * Wrap an e2b `Sandbox` into the injectable `SandboxClient` contract.
 * This is the only place bound to the real SDK.
 */
export function toSandboxClient(sandbox: Sandbox): SandboxClient {
  return {
    async kill() {
      await sandbox.kill();
    },
    commands: {
      async run(command, options) {
        const result = await sandbox.commands.run(command, {
          cwd: options?.cwd,
          envs: options?.env,
          timeoutMs: options?.timeout,
        });
        const out: SandboxCommandResult = {
          stdout: result.stdout,
          stderr: result.stderr,
          exitCode: result.exitCode,
        };
        return out;
      },
    },
    files: {
      async write(path, data) {
        // Normalize every payload to a Blob: the SDK exposes several write
        // overloads and a bare `string | Uint8Array` union is dispatched
        // ambiguously (and `Uint8Array` is not even in its input type), which
        // aborts the upload mid-flight. A Blob pins the byte overload and keeps
        // `string` (UTF-8) and `Uint8Array` (raw view) semantics intact.
        await sandbox.files.write(path, new Blob([data]));
      },
      async read(path) {
        // e2b returns Uint8Array when format:'bytes' is requested.
        return await sandbox.files.read(path, { format: "bytes" });
      },
      async list(path) {
        const entries = await sandbox.files.list(path);
        return entries.map(toFileInfo);
      },
      async makeDir(path) {
        await sandbox.files.makeDir(path);
      },
    },
  };
}

/**
 * Real factory backed by the official e2b SDK pointed at CubeAPI
 * (E2B_API_URL env, default http://127.0.0.1:3000).
 */
export const e2bSandboxFactory: SandboxFactory = {
  async create(options) {
    const sandbox = await Sandbox.create({
      apiKey: options.apiKey,
      timeoutMs: options.timeoutMs,
      template: options.template,
      metadata: { name: "aria-agent" },
      lifecycle: { onTimeout: "kill" },
    });
    return toSandboxClient(sandbox);
  },
};
