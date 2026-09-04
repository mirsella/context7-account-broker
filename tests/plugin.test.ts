import { describe, expect, test } from "bun:test";
import { parseNativeLaunch, server } from "../plugin/context7";

const parsedLaunch = {
	url: "http://127.0.0.1:43210/mcp",
	token: "a".repeat(64),
};
const launch = JSON.stringify(parsedLaunch);

function fakeShell(stdout = launch) {
	const calls: unknown[][] = [];
	const shell = (...args: unknown[]) => {
		calls.push(args);
		const command = {
			quiet: () => command,
			nothrow: async () => ({ exitCode: 0, stdout, stderr: "" }),
		};
		return command;
	};
	return { shell, calls };
}

describe("OpenCode plugin", () => {
	test("parses only safe native launch JSON", () => {
		expect(parseNativeLaunch(launch)).toEqual(parsedLaunch);
		expect(() => parseNativeLaunch(`{"url":"https://example.com/mcp","token":"${"a".repeat(64)}"}`)).toThrow();
		expect(() => parseNativeLaunch(`{"url":"http://127.0.0.1:65536/mcp","token":"${"a".repeat(64)}"}`)).toThrow();
		expect(() => parseNativeLaunch('{"url":"http://127.0.0.1:1/mcp","token":"invalid"}')).toThrow();
		expect(() => parseNativeLaunch(`${launch}\nnoise`)).toThrow();
	});

	test("refuses an existing MCP entry without starting native code", async () => {
		const native = fakeShell();
		const hooks = await server({ $: native.shell } as never);
		const config = { mcp: { "context7-broker": { type: "local" } } };
		await expect(hooks.config!(config as never)).rejects.toThrow("cannot replace");
		expect(native.calls).toEqual([]);
	});

	test("injects the authenticated remote MCP configuration", async () => {
		const native = fakeShell();
		const hooks = await server({ $: native.shell } as never);
		const config: { mcp?: Record<string, unknown> } = {};
		await hooks.config!(config as never);
		expect(native.calls).toHaveLength(1);
		expect(native.calls[0]?.[1]).toEqual([expect.stringContaining("vendor/context7-account-broker"), "start"]);
		expect(config.mcp).toEqual({
			"context7-broker": {
				type: "remote",
				url: "http://127.0.0.1:43210/mcp",
				oauth: false,
				timeout: 30_000,
				headers: {
					Authorization: `Bearer ${"a".repeat(64)}`,
				},
			},
		});
	});
});
