import { e2bSandboxFactory } from "./e2b-client.ts";
import { createContainerSandboxFactory } from "./container-client.ts";
import { KATA_RUNTIME, normalizeSandboxType, type SandboxConfig } from "./config.ts";
import type { SandboxFactory, SandboxPluginDeps } from "./types.ts";

/**
 * Validate and return the effective sandbox backend for a config. The type is
 * already normalized at load time, but overrides (e.g. injected config in
 * plugins) may pass an arbitrary string, so we re-validate defensively and
 * fail loudly on anything unknown.
 */
export function resolveSandboxType(cfg: SandboxConfig) {
  return normalizeSandboxType(String(cfg.sandboxType));
}

/**
 * Pick the sandbox factory for the active backend:
 *   1. an injected `deps.factory` (tests) wins unconditionally;
 *   2. `cubesandbox` uses the legacy e2b/CubeAPI factory;
 *   3. `docker`/`kata` share the OCI-CLI container factory (kata only differs
 *      by the runtime flag, which is carried in the config).
 */
export function getSandboxFactory(
  cfg: SandboxConfig,
  deps: SandboxPluginDeps = {},
): SandboxFactory {
  if (deps.factory) {
    return deps.factory;
  }
  const type = resolveSandboxType(cfg);
  if (type === "cubesandbox") {
    return e2bSandboxFactory;
  }
  // When the backend is kata, default the runtime flag unless one was given
  // explicitly (covers config-override paths that bypass env parsing).
  const runtime = cfg.containerRuntime ?? (type === "kata" ? KATA_RUNTIME : undefined);
  return createContainerSandboxFactory({
    cli: cfg.containerCli,
    runtime,
    image: cfg.containerImage,
    timeoutMs: cfg.timeoutMs,
  });
}
