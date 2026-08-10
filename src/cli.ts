import { createHash } from "node:crypto";

import {
  accountsPath,
  addAccount,
  APP_NAME,
  cachePath,
  loadConfiguredAccounts,
  loadAccounts,
  removeAccount,
} from "./config.js";
import { probeQuota, quotaSummary } from "./pool.js";

const HELP = `Usage: ${APP_NAME} [command]

Commands:
  serve                     Run the MCP server (default)
  status                    Fetch current quota status for every account
  accounts list             List configured accounts and fingerprints
  accounts add [name]       Add an API key using a hidden prompt or stdin
  accounts remove <name>    Remove an account from the config file
  config                    Show effective paths and runtime settings
  help                      Show this help`;

export async function runCli(args: string[]): Promise<boolean> {
  const [command, action, name] = args;
  switch (command) {
    case undefined:
    case "serve":
      return false;
    case "help":
    case "--help":
    case "-h":
      console.log(HELP);
      return true;
    case "status":
      await showStatus();
      return true;
    case "config":
      showConfig();
      return true;
    case "accounts":
      if (action === "list") await listAccounts();
      else if (action === "add") await configureAccount(name);
      else if (action === "remove" && name) {
        await removeAccount(name);
        console.log(`Removed ${name}`);
      } else {
        throw new Error("Use accounts list, accounts add [name], or accounts remove <name>");
      }
      return true;
    default:
      throw new Error(`Unknown command ${command}\n\n${HELP}`);
  }
}

async function showStatus(): Promise<void> {
  const accounts = await loadAccounts();
  if (accounts.length === 0) throw new Error("No accounts configured");
  console.error("Each status check consumes one Context7 API request per account.");
  const statuses = await Promise.all(
    accounts.map(async ({ name, apiKey }) => {
      try {
        return { name, status: quotaSummary(await probeQuota(apiKey)) };
      } catch (error) {
        return { name, status: `ERROR: ${error instanceof Error ? error.message : String(error)}` };
      }
    })
  );
  for (const { name, status } of statuses) console.log(`${name}\t${status}`);
}

async function listAccounts(): Promise<void> {
  const accounts = await loadAccounts();
  if (accounts.length === 0) {
    console.log("No accounts configured");
    return;
  }
  for (const { name, apiKey, source } of accounts) {
    const fingerprint = createHash("sha256").update(apiKey).digest("hex").slice(0, 12);
    console.log(`${name}\t${source}\tsha256:${fingerprint}`);
  }
}

async function configureAccount(requestedName: string | undefined): Promise<void> {
  const configured = await loadConfiguredAccounts();
  const name = requestedName ?? `account-${configured.length + 1}`;
  const apiKey = (await readSecret("Context7 API key: ")).trim();
  if (!apiKey) throw new Error("API key cannot be empty");
  await addAccount(name, apiKey);
  console.log(`Added ${name} to ${accountsPath()}`);
}

function showConfig(): void {
  const ttl = process.env.CONTEXT7_CACHE_TTL_DAYS || "30";
  console.log(`accounts\t${accountsPath()}`);
  console.log(`cache\t${cachePath()}`);
  console.log(`cache TTL\t${ttl} days`);
  console.log(`project\t${process.env.CONTEXT7_PROJECT_ID ?? process.cwd()}`);
  console.log(`upstream\t${process.env.CONTEXT7_MCP_URL ?? "https://mcp.context7.com/mcp"}`);
}

async function readSecret(prompt: string): Promise<string> {
  if (!process.stdin.isTTY) {
    let value = "";
    for await (const chunk of process.stdin) value += chunk;
    return value;
  }

  return new Promise((resolve, reject) => {
    let value = "";
    const stdin = process.stdin;
    const cleanup = (): void => {
      stdin.off("data", onData);
      stdin.setRawMode(false);
      stdin.pause();
      process.stderr.write("\n");
    };
    const onData = (chunk: Buffer): void => {
      for (const byte of chunk) {
        if (byte === 3) {
          cleanup();
          reject(new Error("Cancelled"));
          return;
        }
        if (byte === 10 || byte === 13) {
          cleanup();
          resolve(value);
          return;
        }
        if (byte === 8 || byte === 127) value = value.slice(0, -1);
        else if (byte >= 32 && byte <= 126) value += String.fromCharCode(byte);
      }
    };

    process.stderr.write(prompt);
    stdin.setRawMode(true);
    stdin.resume();
    stdin.on("data", onData);
  });
}
