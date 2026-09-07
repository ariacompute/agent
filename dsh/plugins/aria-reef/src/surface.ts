import type { AriapinClient } from "./ariapin-client.ts";
import type { ArtifactRepo } from "./artifact.ts";
import type { ArtifactRef } from "./types.ts";

export interface SurfaceDeps {
  artifact: ArtifactRepo;
  /** Required only for weight hot reload. */
  ariapin?: AriapinClient;
  logger?: (message: string) => void;
}

export interface ActiveArtifact {
  version: number;
  content: string;
}

export interface Surface {
  /**
   * Read the active version. Deliberately uncached: harness consumers call this
   * every turn so accepted edits take effect without a restart.
   */
  active(ref: ArtifactRef): Promise<ActiveArtifact | null>;
  /** Write + version + commit an accepted candidate. */
  publish(ref: ArtifactRef, content: string, message?: string): Promise<{ version: number; commit: string | null }>;
  /** Ask ariapin to hot-reload served weights (SGLang). */
  reloadWeight(input: { modelId?: string; agentId?: string }): Promise<{ reloaded: boolean; detail: string }>;
}

export function createSurface(deps: SurfaceDeps): Surface {
  const logger = deps.logger ?? ((message: string) => console.warn(`[aria-reef] ${message}`));

  return {
    async active(ref) {
      const [content, version] = await Promise.all([
        deps.artifact.read(ref),
        deps.artifact.activeVersion(ref),
      ]);
      if (content === null) {
        return null;
      }
      return { version, content };
    },
    async publish(ref, content, message) {
      const written = await deps.artifact.write(ref, content);
      const commit = await deps.artifact.commit(
        message ?? `reef: update ${ref.kind}/${ref.name} to v${written.version}`,
      );
      return { version: written.version, commit };
    },
    async reloadWeight(input) {
      if (!deps.ariapin) {
        const detail = "no ariapin client configured; weights stay on the running release";
        logger(detail);
        return { reloaded: false, detail };
      }
      return deps.ariapin.reloadModel(input);
    },
  };
}
