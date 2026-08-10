import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile, readdir, rename, stat, unlink, writeFile } from "node:fs/promises";
import { join } from "node:path";

import { isCallToolResult, type CallToolResult } from "@modelcontextprotocol/server";

interface ResultEntry {
  expiresAt: number;
  result: CallToolResult;
}

interface AffinityEntry {
  expiresAt: number;
  accountKey: string;
}

function canonicalJson(value: unknown): string {
  if (Array.isArray(value)) return `[${value.map(canonicalJson).join(",")}]`;
  if (value === null || typeof value !== "object") return JSON.stringify(value) ?? "null";
  return `{${Object.entries(value as Record<string, unknown>)
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([key, item]) => `${JSON.stringify(key)}:${canonicalJson(item)}`)
    .join(",")}}`;
}

function digest(value: unknown): string {
  return createHash("sha256").update(canonicalJson(value)).digest("hex");
}

export class ResultCache {
  private readonly resultsDirectory: string;
  private readonly affinityDirectory: string;

  private constructor(
    directory: string,
    private readonly ttlMs: number
  ) {
    this.resultsDirectory = join(directory, "results");
    this.affinityDirectory = join(directory, "affinity");
  }

  static async open(directory: string, ttlMs: number): Promise<ResultCache> {
    const cache = new ResultCache(directory, ttlMs);
    await Promise.all([
      mkdir(cache.resultsDirectory, { recursive: true, mode: 0o700 }),
      mkdir(cache.affinityDirectory, { recursive: true, mode: 0o700 }),
    ]);
    await Promise.all([
      cache.removeExpired(cache.resultsDirectory),
      cache.removeExpired(cache.affinityDirectory),
    ]);
    return cache;
  }

  requestKey(scope: string, tool: string, args: Record<string, unknown> | undefined): string {
    return digest({ version: 1, scope, tool, args: args ?? {} });
  }

  async getResult(requestKey: string, accountKey: string): Promise<CallToolResult | undefined> {
    const entry = await this.read<ResultEntry>(
      join(this.resultsDirectory, `${requestKey}-${accountKey}.json`)
    );
    if (!entry || !isCallToolResult(entry.result)) return undefined;
    return entry.result;
  }

  setResult(requestKey: string, accountKey: string, result: CallToolResult): Promise<void> {
    return this.write(join(this.resultsDirectory, `${requestKey}-${accountKey}.json`), {
      expiresAt: Date.now() + this.ttlMs,
      result,
    });
  }

  async getAffinity(scope: string, library: string): Promise<string | undefined> {
    const entry = await this.read<AffinityEntry>(
      join(this.affinityDirectory, `${digest({ version: 1, scope, library })}.json`)
    );
    return typeof entry?.accountKey === "string" ? entry.accountKey : undefined;
  }

  setAffinity(scope: string, library: string, accountKey: string): Promise<void> {
    return this.write(join(this.affinityDirectory, `${digest({ version: 1, scope, library })}.json`), {
      expiresAt: Date.now() + this.ttlMs,
      accountKey,
    });
  }

  private async read<T extends { expiresAt: number }>(file: string): Promise<T | undefined> {
    try {
      const entry = JSON.parse(await readFile(file, "utf8")) as T;
      if (!Number.isFinite(entry.expiresAt) || entry.expiresAt <= Date.now()) {
        await unlink(file).catch(() => undefined);
        return undefined;
      }
      return entry;
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") return undefined;
      console.warn(`[context7-account-broker] Ignoring invalid cache entry ${file}: ${String(error)}`);
      await unlink(file).catch(() => undefined);
      return undefined;
    }
  }

  private async write(file: string, value: unknown): Promise<void> {
    const temporary = `${file}.${process.pid}.${randomUUID()}.tmp`;
    try {
      await writeFile(temporary, JSON.stringify(value), { mode: 0o600 });
      await rename(temporary, file);
    } finally {
      await unlink(temporary).catch(() => undefined);
    }
  }

  private async removeExpired(directory: string): Promise<void> {
    const cutoff = Date.now() - this.ttlMs;
    const files = await readdir(directory);
    await Promise.all(
      files.map(async (file) => {
        const path = join(directory, file);
        try {
          if ((await stat(path)).mtimeMs < cutoff) await unlink(path);
        } catch (error) {
          if ((error as NodeJS.ErrnoException).code !== "ENOENT") throw error;
        }
      })
    );
  }
}
