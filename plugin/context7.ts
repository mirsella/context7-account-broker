import type { Plugin, PluginModule } from "@opencode-ai/plugin";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const BINARY = fileURLToPath(new URL("../vendor/context7-account-broker", import.meta.url));
const MCP_NAME = "context7-broker";

type NativeLaunch = {
	url: string;
	tokenFile: string;
};

export function parseNativeLaunch(stdout: string): NativeLaunch {
	const value: unknown = JSON.parse(stdout);
	if (!value || typeof value !== "object" || Array.isArray(value)) {
		throw new Error("context7-account-broker start returned invalid output");
	}
	const launch = value as Record<string, unknown>;
	if (
		Object.keys(launch).sort().join(",") !== "tokenFile,url" ||
		launch.url !== "http://127.0.0.1:14197/mcp" ||
		typeof launch.tokenFile !== "string" ||
		!launch.tokenFile.startsWith("/")
	) {
		throw new Error("context7-account-broker start returned invalid output");
	}
	return launch as NativeLaunch;
}

export const server: Plugin = async ({ $ }) => ({
	config: async (config) => {
		if (config.mcp?.[MCP_NAME] !== undefined) {
			throw new Error(`@mirsella/context7-account-broker cannot replace the existing mcp.${MCP_NAME} configuration`);
		}
		const result = await $`${[BINARY, "start"]}`.quiet().nothrow();
		if (result.exitCode !== 0) {
			const stderr = String(result.stderr).trim();
			throw new Error(`context7-account-broker start failed with exit code ${result.exitCode}${stderr ? `: ${stderr}` : ""}`);
		}
		const launch = parseNativeLaunch(String(result.stdout));
		const token = (await readFile(launch.tokenFile, "utf8")).trim();
		if (!/^[0-9a-f]{64}$/.test(token)) {
			throw new Error("context7-account-broker server token is invalid");
		}
		config.mcp ??= {};
		config.mcp[MCP_NAME] = {
			type: "remote",
			url: launch.url,
			oauth: false,
			timeout: 30_000,
			headers: {
				Authorization: `Bearer ${token}`,
			},
		};
	},
});

export default { id: "@mirsella/context7-account-broker", server } satisfies PluginModule;
