import { mkdir, readFile, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { defaultRunCommand, type RunCommand } from "../../shared/src/index.ts";
import { AriaError, ErrorCode } from "../../shared/src/error.ts";
import { LFS_PATTERNS } from "./config.ts";
import { artifactKey, type ArtifactRef } from "./types.ts";

export interface ArtifactRepoOptions {
  /** Root of the git repo that version-controls evolved artifacts. */
  root: string;
  /** Injected git runner (tests use a fake). */
  runCommand?: RunCommand;
  logger?: (message: string) => void;
}

export interface WrittenArtifact {
  version: number;
  path: string;
  historyPath: string;
}

export interface ArtifactRepo {
  root: string;
  /** `git init` + LFS attributes + local identity (idempotent). */
  init(): Promise<void>;
  read(ref: ArtifactRef): Promise<string | null>;
  /** Write a new active version and snapshot the previous history. */
  write(ref: ArtifactRef, content: string): Promise<WrittenArtifact>;
  activeVersion(ref: ArtifactRef): Promise<number>;
  readVersion(ref: ArtifactRef, version: number): Promise<string | null>;
  /** `git add -A && git commit`. Returns the sha, or null when nothing changed. */
  commit(message: string): Promise<string | null>;
  log(limit?: number): Promise<string[]>;
}

const STATE_FILE = "reef-state.json";
const ATTRIBUTES_FILE = ".gitattributes";

/** Artifact names are repo-relative-ish paths; keep them tame. */
export function assertArtifactName(name: string): string {
  const trimmed = name.trim().replace(/^\/+/, "");
  if (trimmed.length === 0) {
    throw new AriaError("artifact name must not be empty", ErrorCode.INVALID_PARAM);
  }
  if (!/^[A-Za-z0-9._\-/]+$/.test(trimmed) || trimmed.includes("..") || trimmed.includes("//")) {
    throw new AriaError(
      `invalid artifact name "${name}" (allowed: [A-Za-z0-9._-/], no "..")`,
      ErrorCode.INVALID_PARAM,
    );
  }
  return trimmed.replace(/\/+/g, "/");
}

function refPathParts(ref: ArtifactRef): { kind: string; name: string } {
  return { kind: ref.kind, name: assertArtifactName(ref.name) };
}

async function readJson<T>(file: string, fallback: T): Promise<T> {
  try {
    return JSON.parse(await readFile(file, "utf8")) as T;
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return fallback;
    }
    throw new AriaError(
      `reef artifact: cannot read ${file}: ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.REEF_STORE,
    );
  }
}

async function readText(file: string): Promise<string | null> {
  try {
    return await readFile(file, "utf8");
  } catch (err) {
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      return null;
    }
    throw new AriaError(
      `reef artifact: cannot read ${file}: ${err instanceof Error ? err.message : String(err)}`,
      ErrorCode.REEF_STORE,
    );
  }
}

export function createArtifactRepo(options: ArtifactRepoOptions): ArtifactRepo {
  const run = options.runCommand ?? defaultRunCommand;
  const logger = options.logger ?? ((message: string) => console.warn(`[aria-reef] ${message}`));
  const stateFile = join(options.root, STATE_FILE);

  async function git(args: string[], allowFailure = false): Promise<{ code: number; stdout: string; stderr: string }> {
    const result = await run("git", ["-C", options.root, ...args]);
    if (result.code !== 0 && !allowFailure) {
      throw new AriaError(
        `reef artifact: git ${args.join(" ")} failed (${result.code}): ${result.stderr.trim() || result.stdout.trim()}`,
        ErrorCode.REEF_GIT,
      );
    }
    return result;
  }

  async function writeState(versions: Record<string, number>): Promise<void> {
    await mkdir(options.root, { recursive: true });
    await writeFile(stateFile, `${JSON.stringify({ versions }, null, 2)}\n`, "utf8");
  }

  return {
    root: options.root,
    async init() {
      await mkdir(options.root, { recursive: true });
      const attributes = `${LFS_PATTERNS.map((pattern) => `${pattern} filter=lfs diff=lfs merge=lfs -text`).join("\n")}\n`;
      await writeFile(join(options.root, ATTRIBUTES_FILE), attributes, "utf8");
      await git(["init"]);
      await git(["config", "user.email", "aria-reef@localhost"]);
      await git(["config", "user.name", "aria-reef"]);
      const lfs = await git(["lfs", "install", "--local"], true);
      if (lfs.code !== 0) {
        logger(`git-lfs unavailable (code ${lfs.code}); weight artifacts will be committed directly`);
      }
      const state = await readJson<{ versions?: Record<string, number> }>(stateFile, {});
      if (!state.versions) {
        await writeState({});
      }
    },
    async read(ref) {
      const { kind, name } = refPathParts(ref);
      return readText(join(options.root, "artifacts", kind, name));
    },
    async write(ref, content) {
      const { kind, name } = refPathParts(ref);
      const key = artifactKey({ kind: ref.kind, name });
      const state = await readJson<{ versions?: Record<string, number> }>(stateFile, {});
      const versions = state.versions ?? {};
      const version = (versions[key] ?? 0) + 1;
      const activePath = join(options.root, "artifacts", kind, name);
      const historyPath = join(options.root, "history", kind, name, `v${version}`);
      await mkdir(join(activePath, ".."), { recursive: true });
      await mkdir(join(historyPath, ".."), { recursive: true });
      await writeFile(activePath, content, "utf8");
      await writeFile(historyPath, content, "utf8");
      versions[key] = version;
      await writeState(versions);
      return { version, path: activePath, historyPath };
    },
    async activeVersion(ref) {
      const { name } = refPathParts(ref);
      const state = await readJson<{ versions?: Record<string, number> }>(stateFile, {});
      return state.versions?.[artifactKey({ kind: ref.kind, name })] ?? 0;
    },
    async readVersion(ref, version) {
      const { kind, name } = refPathParts(ref);
      return readText(join(options.root, "history", kind, name, `v${version}`));
    },
    async commit(message) {
      await git(["add", "-A"]);
      const result = await git(["commit", "-m", message], true);
      if (result.code !== 0) {
        logger(`git commit skipped: ${result.stderr.trim() || result.stdout.trim() || "nothing to commit"}`);
        return null;
      }
      const sha = await git(["rev-parse", "HEAD"], true);
      return sha.code === 0 ? sha.stdout.trim() || null : null;
    },
    async log(limit = 20) {
      const result = await git(["log", "--oneline", "-n", String(limit)], true);
      if (result.code !== 0) {
        return [];
      }
      return result.stdout
        .split("\n")
        .map((line) => line.trim())
        .filter((line) => line.length > 0);
    },
  };
}
