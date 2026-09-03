import { describe, expect, test } from "bun:test";
import { mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { parseNativeLaunch, server } from "../plugin/context7";

const parsedLaunch = {
	url: "http://127.0.0.1:14197/mcp",
	tokenFile: "/tmp/context7/server-token",
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
		expect(() => parseNativeLaunch('{"url":"https://example.com/mcp","tokenFile":"/tmp/t"}')).toThrow();
		expect(() => parseNativeLaunch('{"url":"http://127.0.0.1:1/mcp","tokenFile":"relative"}')).toThrow();
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
		const directory = await mkdtemp(join(tmpdir(), "context7-plugin-"));
		try {
			const tokenFile = join(directory, "server-token");
			await writeFile(tokenFile, "a".repeat(64));
			const native = fakeShell(JSON.stringify({ ...parsedLaunch, tokenFile }));
			const hooks = await server({ $: native.shell } as never);
			const config: { mcp?: Record<string, unknown> } = {};
			await hooks.config!(config as never);
			expect(native.calls).toHaveLength(1);
			expect(native.calls[0]?.[1]).toEqual([expect.stringContaining("vendor/context7-account-broker"), "start"]);
			expect(config.mcp).toEqual({
				"context7-broker": {
					type: "remote",
					url: "http://127.0.0.1:14197/mcp",
					oauth: false,
					timeout: 30_000,
					headers: {
						Authorization: `Bearer ${"a".repeat(64)}`,
					},
				},
			});
		} finally {
			await rm(directory, { recursive: true });
		}
	});
});
