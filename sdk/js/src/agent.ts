/**
 * `Agent` — mirrors `new Agent({ name, instructions, model, tools, handoffs })`
 * from the OpenAI Agents SDK.
 */

import type { AgentConfig, ClientOptions, MemoryConfig, ToolSpec } from "./types.js";

export class Agent {
  readonly name: string;
  readonly instructions?: string;
  readonly model?: string;
  readonly tools: ToolSpec[];
  readonly handoffs: Agent[];
  readonly handoffDescription?: string;
  readonly id?: string;
  readonly client: ClientOptions;
  /** Memory backend configuration (`cloud` / `local` / `both`). */
  readonly memory: MemoryConfig;

  constructor(config: AgentConfig) {
    if (!config.name || !config.name.trim()) {
      throw new Error("Agent requires a `name`");
    }
    this.name = config.name;
    this.instructions = config.instructions;
    this.model = config.model;
    this.tools = config.tools ?? [];
    this.handoffs = config.handoffs ?? [];
    this.handoffDescription = config.handoffDescription;
    this.id = config.id;
    this.client = config.client ?? {};
    this.memory = config.memory ?? {};
  }

  /** Static factory, matching `Agent.create({...})` in the Agents SDK. */
  static create(config: AgentConfig): Agent {
    return new Agent(config);
  }

  /** Clone with overrides (used when a handoff takes over a run). */
  clone(overrides: Partial<AgentConfig> = {}): Agent {
    return new Agent({
      name: overrides.name ?? this.name,
      instructions: overrides.instructions ?? this.instructions,
      model: overrides.model ?? this.model,
      tools: overrides.tools ?? this.tools,
      handoffs: overrides.handoffs ?? this.handoffs,
      handoffDescription: overrides.handoffDescription ?? this.handoffDescription,
      id: overrides.id ?? this.id,
      client: overrides.client ?? this.client,
      memory: overrides.memory ?? this.memory,
    });
  }

  /** JSON-Schema tool declarations forwarded to the cloud agent. */
  toolSchemas(): ToolSpec[] {
    return this.tools.map((t) => ({
      name: t.name,
      description: t.description,
      parameters: t.parameters ?? { type: "object", properties: {} },
    }));
  }
}
