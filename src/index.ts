#!/usr/bin/env node

import { createHash } from "node:crypto";

import { Client, StreamableHTTPClientTransport } from "@modelcontextprotocol/client";
import {
  McpServer,
  type CallToolResult,
  type ListToolsResult,
} from "@modelcontextprotocol/server";
import { serveStdio } from "@modelcontextprotocol/server/stdio";

import { ResultCache } from "./cache.js";
import { runCli } from "./cli.js";
import { APP_NAME, cachePath, loadAccounts } from "./config.js";
import {
  AccountPool,
  probeQuota,
  quotaSummary,
  type Account,
} from "./pool.js";

const VERSION = "0.1.0";

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function createServer(pool: AccountPool): McpServer {
  const server = new McpServer(
    {
      name: "Context7",
      version: VERSION,
      websiteUrl: "https://context7.com",
      description: "A multi-account forwarding proxy for Context7 documentation tools.",
    },
    {
      instructions:
        "Use this server exactly like Context7. Account selection and failover are automatic.",
    }
  );

  // Low-level handlers preserve the upstream schemas and results without translation.
  server.server.registerCapabilities({ tools: {} });
  server.server.setRequestHandler(
    "tools/list",
    async (): Promise<ListToolsResult> => ({ tools: [...pool.tools] })
  );
  server.server.setRequestHandler(
    "tools/call",
    async ({ params }): Promise<CallToolResult> =>
      pool.callTool(params.name, params.arguments)
  );
  return server;
}

async function main(): Promise<void> {
  if (await runCli(process.argv.slice(2))) return;

  const credentials = await loadAccounts();
  if (credentials.length === 0) {
    throw new Error(
      `No accounts configured. Run ${APP_NAME} accounts add or set CONTEXT7_API_KEYS`
    );
  }

  const endpoint = new URL(process.env.CONTEXT7_MCP_URL ?? "https://mcp.context7.com/mcp");
  if (endpoint.protocol !== "http:" && endpoint.protocol !== "https:") {
    throw new Error("CONTEXT7_MCP_URL must use HTTP or HTTPS");
  }

  const cooldownMs = Number(process.env.CONTEXT7_ACCOUNT_COOLDOWN_MS || 30_000);
  if (!Number.isInteger(cooldownMs) || cooldownMs < 0) {
    throw new Error("CONTEXT7_ACCOUNT_COOLDOWN_MS must be a non-negative integer");
  }

  const cacheTtlDays = Number(process.env.CONTEXT7_CACHE_TTL_DAYS || 30);
  if (!Number.isFinite(cacheTtlDays) || cacheTtlDays <= 0) {
    throw new Error("CONTEXT7_CACHE_TTL_DAYS must be a positive number");
  }
  const cache = await ResultCache.open(cachePath(), cacheTtlDays * 86_400_000);
  const projectScope = process.env.CONTEXT7_PROJECT_ID ?? process.cwd();

  const connected = await Promise.all(
    credentials.map(async ({ name: id, apiKey }): Promise<Account | undefined> => {
      const client = new Client({ name: APP_NAME, version: VERSION });
      const transport = new StreamableHTTPClientTransport(endpoint, {
        authProvider: { token: async () => apiKey },
        onInsufficientScope: "throw",
         requestInit: { headers: { "User-Agent": `${APP_NAME}/${VERSION}` } },
      });

      try {
        const quota = await probeQuota(apiKey);
        await client.connect(transport);
        const { tools } = await client.listTools();
        if (tools.length === 0) throw new Error("upstream advertised no tools");
         console.error(`[${APP_NAME}] ${id} quota: ${quotaSummary(quota)}`);
        return {
          id,
          accountKey: createHash("sha256").update(apiKey).digest("hex"),
          client,
          tools,
          quota,
          refreshQuota: () => probeQuota(apiKey),
        };
      } catch (error) {
         console.error(`[${APP_NAME}] Skipping ${id}: ${message(error)}`);
        await client.close().catch((closeError) =>
           console.error(`[${APP_NAME}] Failed to close ${id}: ${message(closeError)}`)
        );
      }
    })
  );
  const accounts = connected.filter((account): account is Account => account !== undefined);
  let pool: AccountPool;
  try {
    pool = new AccountPool(accounts, cooldownMs, cache, projectScope);
  } catch (error) {
    await Promise.allSettled(accounts.map(({ client }) => client.close()));
    throw error;
  }

  console.error(
     `[${APP_NAME}] Connected ${accounts.length} account(s); forwarding ${pool.tools
      .map(({ name }) => name)
      .join(", ")}`
  );

  const handle = serveStdio(() => createServer(pool), {
     onerror: (error) => console.error(`[${APP_NAME}] MCP error: ${message(error)}`),
  });
  let closing: Promise<void> | undefined;
  const shutdown = (): Promise<void> =>
    (closing ??= Promise.all([handle.close(), pool.close()]).then(() => undefined));
  const requestShutdown = (): void => {
     void shutdown().catch((error) => console.error(`[${APP_NAME}] Shutdown failed: ${message(error)}`));
  };

  process.once("SIGINT", requestShutdown);
  process.once("SIGTERM", requestShutdown);
  process.stdin.once("end", requestShutdown);
}

void main().catch((error) => {
  console.error(`[${APP_NAME}] Fatal error: ${message(error)}`);
  process.exitCode = 1;
});
