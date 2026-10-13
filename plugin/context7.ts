import type { Plugin, PluginModule } from "@opencode-ai/plugin";
import { fileURLToPath } from "node:url";

const BINARY = fileURLToPath(new URL("../vendor/context7-account-broker", import.meta.url));
const MCP_NAME = "context7-broker";

export const server: Plugin = async () => ({
	config: async (config) => {
		if (config.mcp?.[MCP_NAME] !== undefined) {
			throw new Error(`@mirsella/context7-account-broker cannot replace the existing mcp.${MCP_NAME} configuration`);
		}
		config.mcp ??= {};
		config.mcp[MCP_NAME] = {
			type: "local",
			command: [BINARY, "mcp"],
			enabled: true,
			timeout: 30_000,
		};
	},
});

export default { id: "@mirsella/context7-account-broker", server } satisfies PluginModule;
