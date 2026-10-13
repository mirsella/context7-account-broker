import { describe, expect, test } from "bun:test";
import { server } from "../plugin/context7";

describe("OpenCode plugin", () => {
	test("refuses an existing MCP entry", async () => {
		const hooks = await server({} as never);
		const config = { mcp: { "context7-broker": { type: "local" } } };
		await expect(hooks.config!(config as never)).rejects.toThrow("cannot replace");
		expect(config.mcp["context7-broker"]).toEqual({ type: "local" });
	});

	test("registers the native MCP command without launching a broker during configuration", async () => {
		const hooks = await server({} as never);
		const config: { mcp?: Record<string, unknown> } = {};
		await hooks.config!(config as never);
		expect(config.mcp).toEqual({
			"context7-broker": {
				type: "local",
				command: [expect.stringContaining("vendor/context7-account-broker"), "mcp"],
				enabled: true,
				timeout: 30_000,
			},
		});
	});
});
