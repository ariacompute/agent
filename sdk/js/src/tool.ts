/**
 * `tool()` — mirrors `tool({ name, description, parameters, execute })` from the
 * OpenAI Agents SDK.
 *
 * The cloud executes tools inside its own sandbox, so only the schema
 * (`name` / `description` / `parameters`) is forwarded to the server. `execute`
 * is kept for API compatibility and for local/hosted runners; it is never
 * called by the cloud transport.
 */

import type { ToolSpec } from "./types.js";

export function tool(spec: ToolSpec): ToolSpec {
  if (!spec.name || !spec.name.trim()) {
    throw new Error("tool requires a `name`");
  }
  return {
    name: spec.name,
    description: spec.description ?? "",
    parameters: spec.parameters ?? { type: "object", properties: {} },
    execute: spec.execute,
  };
}
