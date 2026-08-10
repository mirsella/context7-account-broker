import { mkdtemp, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { CallToolResult, Tool } from "@modelcontextprotocol/server";

import { ResultCache } from "../src/cache.js";
import { AccountPool, probeQuota, type Account, type Quota } from "../src/pool.js";

const tool: Tool = {
  name: "query-docs",
  inputSchema: { type: "object" },
};
const tools: readonly Tool[] = [
  tool,
  { name: "resolve-library-id", inputSchema: { type: "object" } },
];

function success(text: string): CallToolResult {
  return { content: [{ type: "text", text }] };
}

function quota(remaining = 100, overrides: Partial<Quota> = {}): Quota {
  return {
    limit: 100,
    remaining,
    updatedAt: Date.now(),
    resetAt: Date.now() + 86_400_000,
    blocked: false,
    ...overrides,
  };
}

function fakeAccount(
  id: string,
  callTool: Account["client"]["callTool"],
  options: {
    tools?: readonly Tool[];
    quota?: Quota;
    refreshQuota?: () => Promise<Quota>;
  } = {}
): Account {
  const initialQuota = options.quota ?? quota();
  return {
    id,
    accountKey: id,
    tools: options.tools ?? tools,
    quota: initialQuota,
    refreshQuota: options.refreshQuota ?? (async () => initialQuota),
    client: {
      callTool,
      close: async () => {},
    },
  };
}

describe("AccountPool", () => {
  let directory: string;
  let cache: ResultCache;
  const pool = (accounts: Account[], cooldownMs = 1000): AccountPool =>
    new AccountPool(accounts, cooldownMs, cache, "project");

  beforeEach(async () => {
    directory = await mkdtemp(join(tmpdir(), "context7-account-broker-"));
    cache = await ResultCache.open(directory, 60_000);
  });

  afterEach(async () => {
    vi.unstubAllGlobals();
    vi.useRealTimers();
    await rm(directory, { recursive: true, force: true });
  });

  it("canonicalizes arguments and expires cached results", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date("2026-08-10T00:00:00Z"));
    const expiring = await ResultCache.open(join(directory, "expiring"), 1000);
    const firstKey = expiring.requestKey("project", "query-docs", { b: 2, a: 1 });
    const secondKey = expiring.requestKey("project", "query-docs", { a: 1, b: 2 });

    await expiring.setResult(firstKey, "account", success("cached"));

    expect(secondKey).toBe(firstKey);
    expect(await expiring.getResult(secondKey, "account")).toEqual(success("cached"));
    vi.advanceTimersByTime(1001);
    expect(await expiring.getResult(secondKey, "account")).toBeUndefined();
  });

  it("reads authoritative quota state from Context7 headers", async () => {
    const reset = 1_788_220_800;
    const fetchMock = vi.fn().mockResolvedValue(
      new Response(null, {
        status: 200,
        headers: {
          "RateLimit-Limit": "1000",
          "RateLimit-Remaining": "742",
          "RateLimit-Reset": String(reset),
        },
      })
    );
    vi.stubGlobal("fetch", fetchMock);

    const result = await probeQuota("secret");

    expect(result).toMatchObject({
      limit: 1000,
      remaining: 742,
      resetAt: reset * 1000,
      blocked: false,
    });
    expect(fetchMock).toHaveBeenCalledWith(
      expect.any(URL),
      expect.objectContaining({ headers: { Authorization: "Bearer secret" } })
    );
  });

  it("keeps different queries for one library on the same account", async () => {
    const calls: string[] = [];
    const broker = pool(
      [
        fakeAccount("one", async () => {
          calls.push("one");
          return success("one");
        }),
        fakeAccount("two", async () => {
          calls.push("two");
          return success("two");
        }),
      ]
    );

    await broker.callTool("query-docs", { libraryId: "/bevyengine/bevy", query: "first" });
    await broker.callTool("query-docs", { libraryId: "/bevyengine/bevy", query: "second" });

    expect(calls).toEqual(["one", "one"]);
  });

  it("serves exact calls from the persistent cache", async () => {
    let calls = 0;
    const accounts = [
      fakeAccount("one", async () => {
        calls += 1;
        return success("cached");
      }),
    ];
    const args = { libraryId: "/bevyengine/bevy", query: "systems" };

    await pool(accounts).callTool("query-docs", args);
    const result = await pool(accounts).callTool("query-docs", args);

    expect(calls).toBe(1);
    expect(result).toEqual(success("cached"));
  });

  it("binds resolved library IDs to the search account", async () => {
    const calls: string[] = [];
    const broker = pool([
      fakeAccount("one", async ({ name }) => {
        calls.push(`one:${name}`);
        return name === "resolve-library-id"
          ? success("Context7-compatible library ID: /bevyengine/bevy")
          : success("docs");
      }),
      fakeAccount("two", async ({ name }) => {
        calls.push(`two:${name}`);
        return success("docs");
      }),
    ]);

    await broker.callTool("resolve-library-id", { libraryName: "Bevy", query: "game engine" });
    await broker.callTool("query-docs", { libraryId: "/bevyengine/bevy", query: "systems" });

    expect(calls).toEqual(["one:resolve-library-id", "one:query-docs"]);
  });

  it("fails over when an account returns a quota or authentication failure", async () => {
    const calls: string[] = [];
    const broker = pool(
      [
        fakeAccount("limited", async () => {
          calls.push("limited");
          return success("Rate limited or quota exceeded. Upgrade your plan.");
        }),
        fakeAccount("healthy", async () => {
          calls.push("healthy");
          return success("documentation");
        }),
      ]
    );

    const result = await broker.callTool("query-docs", { query: "docs" });

    expect(calls).toEqual(["limited", "healthy"]);
    expect(result).toEqual(success("documentation"));
  });

  it("routes to the account with the lowest proportional usage", async () => {
    const calls: string[] = [];
    const broker = pool(
      [
        fakeAccount("busy", async () => success("busy"), { quota: quota(20) }),
        fakeAccount(
          "idle",
          async () => {
            calls.push("idle");
            return success("idle");
          },
          { quota: quota(90) }
        ),
      ]
    );

    await broker.callTool("query-docs", undefined);

    expect(calls).toEqual(["idle"]);
  });

  it("refreshes blocked accounts after their reset", async () => {
    let refreshes = 0;
    const broker = pool(
      [
        fakeAccount("reset", async () => success("ready"), {
          quota: quota(0, { blocked: true, resetAt: Date.now() - 1 }),
          refreshQuota: async () => {
            refreshes += 1;
            return quota(99);
          },
        }),
      ]
    );

    const result = await broker.callTool("query-docs", undefined);

    expect(refreshes).toBe(1);
    expect(result).toEqual(success("ready"));
  });

  it("returns a visible error when every account is unavailable", async () => {
    const broker = pool(
      [
        fakeAccount("one", async () => {
          throw new TypeError("fetch failed", { cause: { code: "ECONNRESET" } });
        }),
      ]
    );

    const result = await broker.callTool("query-docs", undefined);

    expect(result.isError).toBe(true);
    expect(result.content[0]).toMatchObject({ type: "text" });
  });

  it("rejects accounts with incompatible tool contracts", () => {
    const incompatible = { ...tool, description: "different" };

    expect(
      () =>
        pool(
          [
            fakeAccount("one", async () => success("one")),
            fakeAccount("two", async () => success("two"), { tools: [incompatible] }),
          ]
        )
    ).toThrow("two advertised tools that differ from one");
  });
});
