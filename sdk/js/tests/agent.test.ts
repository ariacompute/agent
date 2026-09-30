import { describe, expect, test } from "bun:test";
import { Agent, tool } from "../src/index.js";

describe("Agent construction", () => {
  test("rejects empty or whitespace-only names", () => {
    expect(() => new Agent({ name: "" })).toThrow();
    expect(() => new Agent({ name: "   " })).toThrow();
  });

  test("rejects duplicate tool names", () => {
    const t = tool({ name: "lookup", parameters: { type: "object" } });
    expect(() => new Agent({ name: "tutor", tools: [t, t] })).toThrow(/duplicate tool name/);
  });

  test("defaults tools and handoffs to empty, preserves memory config", () => {
    const agent = new Agent({ name: "tutor", memory: { backend: "both" } });
    expect(agent.tools).toEqual([]);
    expect(agent.handoffs).toEqual([]);
    expect(agent.memory.backend).toBe("both");
  });

  test("toolSchemas omit execute and default parameters", () => {
    const t = tool({ name: "lookup" });
    const agent = new Agent({ name: "tutor", tools: [t] });
    const schemas = agent.toolSchemas();
    expect(schemas[0]).not.toHaveProperty("execute");
    expect(schemas[0].parameters).toEqual({ type: "object", properties: {} });
  });

  test("clone overrides only the given fields", () => {
    const base = new Agent({ name: "tutor", instructions: "be brief", memory: { backend: "cloud" } });
    const next = base.clone({ instructions: "be verbose" });
    expect(next.name).toBe("tutor");
    expect(next.instructions).toBe("be verbose");
    expect(next.memory.backend).toBe("cloud");
  });
});
