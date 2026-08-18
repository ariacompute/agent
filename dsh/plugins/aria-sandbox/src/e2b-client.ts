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

function toClient(sandbox: Sandbox): SandboxClient {
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
        await sandbox.files.write(path, data);
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
    return toClient(sandbox);
  },
};
