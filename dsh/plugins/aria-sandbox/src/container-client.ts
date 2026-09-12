import { spawn } from "node:child_process";
import { mkdir, readdir, readFile, stat, writeFile } from "node:fs/promises";
import { dirname, join, posix } from "node:path";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import {
  assertSandboxPath,
  SANDBOX_WORKSPACE_DIR,
} from "./workspace.ts";
import {
  DEFAULT_SANDBOX_CLI,
  DEFAULT_SANDBOX_IMAGE,
  DEFAULT_TIMEOUT_MS,
} from "./config.ts";
import type {
  ContainerExecutor,
  SandboxClient,
  SandboxCreateOptions,
  SandboxFactory,
} from "./types.ts";

/**
 * Production executor backed by the OCI CLI (`docker`/`nerdctl`) via spawn.
 * `run` receives the subcommand + flags (without the binary); it returns the
 * trimmed stdout/stderr and the process exit code so callers can decide
 * whether a non-zero code is a command-level result or a process-level error.
 */
export function nodeSpawnExecutor(cli: string): ContainerExecutor {
  return {
    run(args: string[]): Promise<{ stdout: string; stderr: string; exitCode: number }> {
      return new Promise((resolve, reject) => {
        const child = spawn(cli, args, { stdio: ["ignore", "pipe", "pipe"] });
        let stdout = "";
        let stderr = "";
        child.stdout?.on("data", (chunk) => {
          stdout += chunk.toString();
        });
        child.stderr?.on("data", (chunk) => {
          stderr += chunk.toString();
        });
        child.on("error", (err) => {
          reject(err);
        });
        child.on("close", (code) => {
          resolve({
            stdout: stdout.trim(),
            stderr: stderr.trim(),
            exitCode: code ?? 0,
          });
        });
      });
    },
  };
}

export interface ContainerSandboxFactoryDeps {
  /** Injectable executor (tests use a fake). Defaults to nodeSpawnExecutor(cli). */
  executor?: ContainerExecutor;
  /** OCI CLI binary. */
  cli?: string;
  /** Container runtime (kata => "kata"). */
  runtime?: string;
  /** Container image. */
  image?: string;
  /** Default command timeout in ms. */
  timeoutMs?: number;
}

/**
 * Factory for the docker/kata backend. A single long-lived container is created
 * per workspace: the host workspace dir is bind-mounted at /workspace, file
 * operations go straight to the host filesystem (reusing the same /workspace
 * isolation rules as the rest of the plugin), and shell commands run via
 * `docker exec`. kata is identical except for the --runtime flag.
 */
export function createContainerSandboxFactory(
  deps: ContainerSandboxFactoryDeps = {},
): SandboxFactory {
  const cli = deps.cli ?? DEFAULT_SANDBOX_CLI;
  const runtime = deps.runtime;
  const image = deps.image ?? DEFAULT_SANDBOX_IMAGE;
  const timeoutMs = deps.timeoutMs ?? DEFAULT_TIMEOUT_MS;
  const executor = deps.executor ?? nodeSpawnExecutor(cli);

  function translate(hostDir: string, remotePath: string): string {
    // Re-assert the path stays under /workspace, then map it onto the host dir.
    const normalized = assertSandboxPath(remotePath);
    const rel = posix.relative(SANDBOX_WORKSPACE_DIR, normalized);
    return rel.length === 0 ? hostDir : join(hostDir, rel);
  }

  return {
    async create(options: SandboxCreateOptions): Promise<SandboxClient> {
      const hostDir = options.hostDir;
      if (!hostDir) {
        throw new AriaError(
          "container sandbox requires a hostDir to bind-mount",
          ErrorCode.SANDBOX,
        );
      }

      const createArgs = ["create", "--rm=false"];
      if (runtime) {
        createArgs.push("--runtime", runtime);
      }
      createArgs.push(
        "-v",
        `${hostDir}:${SANDBOX_WORKSPACE_DIR}`,
        "-w",
        SANDBOX_WORKSPACE_DIR,
        options.image ?? image,
        "tail",
        "-f",
        "/dev/null",
      );

      const created = await executor.run(createArgs);
      if (created.exitCode !== 0) {
        throw new AriaError(
          `container create failed: ${created.stderr || created.stdout}`,
          ErrorCode.SANDBOX,
        );
      }
      const cid = created.stdout.trim();
      if (!cid) {
        throw new AriaError("container create returned no id", ErrorCode.SANDBOX);
      }

      return {
        async kill() {
          await executor.run(["rm", "-f", cid]);
        },
        commands: {
          async run(command, runOptions) {
            const cwd = runOptions?.cwd ?? SANDBOX_WORKSPACE_DIR;
            const timeout = runOptions?.timeout ?? timeoutMs;
            const execArgs = [
              "exec",
              "-w",
              cwd,
              cid,
              "sh",
              "-c",
              command,
            ];
            const args =
              timeout && timeout > 0
                ? ["timeout", `${Math.ceil(timeout / 1000)}s`, ...execArgs]
                : execArgs;
            const result = await executor.run(args);
            return {
              stdout: result.stdout,
              stderr: result.stderr,
              exitCode: result.exitCode,
            };
          },
        },
        files: {
          async write(path, data) {
            const abs = translate(hostDir, path);
            const buf = typeof data === "string" ? Buffer.from(data, "utf8") : Buffer.from(data);
            await mkdir(dirname(abs), { recursive: true });
            await writeFile(abs, buf);
          },
          async read(path) {
            const abs = translate(hostDir, path);
            return new Uint8Array(await readFile(abs));
          },
          async list(path) {
            const target = path && path.length > 0 ? path : SANDBOX_WORKSPACE_DIR;
            const abs = translate(hostDir, target);
            const entries = await readdir(abs, { withFileTypes: true });
            return entries.map((entry) => ({
              name: entry.name,
              path: posix.join(target, entry.name),
              isDir: entry.isDirectory(),
            }));
          },
          async makeDir(path) {
            const abs = translate(hostDir, path);
            await mkdir(abs, { recursive: true });
          },
        },
      };
    },
  };
}
