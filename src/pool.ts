import { isDeepStrictEqual } from "node:util";

import {
  type Client,
  InsufficientScopeError,
  SdkError,
  SdkErrorCode,
  SdkHttpError,
  UnauthorizedError,
} from "@modelcontextprotocol/client";
import {
  ProtocolError,
  ProtocolErrorCode,
  type CallToolResult,
  type Tool,
} from "@modelcontextprotocol/server";

import { ResultCache } from "./cache.js";

export interface Quota {
  limit: number;
  remaining: number;
  updatedAt: number;
  resetAt: number;
  blocked: boolean;
}

export interface Account {
  readonly id: string;
  readonly accountKey: string;
  readonly client: Pick<Client, "callTool" | "close">;
  readonly tools: readonly Tool[];
  quota: Quota;
  readonly refreshQuota: () => Promise<Quota>;
}

interface AccountState extends Account {
  readonly index: number;
  cooldownUntil: number;
  quotaRefreshAt: number;
  refreshing?: Promise<void>;
}

const RETRYABLE_HTTP_STATUSES = new Set([401, 403, 408, 425, 429, 500, 502, 503, 504]);
const RETRYABLE_SDK_ERRORS = new Set([
  SdkErrorCode.NotConnected,
  SdkErrorCode.RequestTimeout,
  SdkErrorCode.ConnectionClosed,
  SdkErrorCode.SendFailed,
]);
const RETRYABLE_NETWORK_ERRORS = new Set([
  "EAI_AGAIN",
  "ECONNREFUSED",
  "ECONNRESET",
  "EHOSTUNREACH",
  "ENETUNREACH",
  "ENOTFOUND",
  "ETIMEDOUT",
  "UND_ERR_CONNECT_TIMEOUT",
  "UND_ERR_HEADERS_TIMEOUT",
  "UND_ERR_SOCKET",
]);
const QUOTA_RESULT = /^(?:monthly quota reached|rate limited or quota exceeded)/i;
const RETRYABLE_RESULT = /^(?:monthly quota reached|rate limited or quota exceeded|invalid api key|authentication required|request failed with status (?:40[1238]|425|429|5\d\d)\b|error (?:searching libraries:|fetching library context\.))/i;

function message(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}

function resultText(result: CallToolResult): string {
  return result.content
    .flatMap((block) => (block.type === "text" ? [block.text] : []))
    .join("\n");
}

function isRetryableError(error: unknown): boolean {
  if (error instanceof UnauthorizedError || error instanceof InsufficientScopeError) return true;
  if (error instanceof SdkHttpError) return RETRYABLE_HTTP_STATUSES.has(error.status);
  if (error instanceof SdkError) return RETRYABLE_SDK_ERRORS.has(error.code);

  const cause = error instanceof Error ? error.cause : undefined;
  const code =
    cause && typeof cause === "object" && "code" in cause ? String(cause.code) : undefined;
  return code !== undefined && RETRYABLE_NETWORK_ERRORS.has(code);
}

function brokerError(text: string): CallToolResult {
  return {
    content: [{ type: "text", text: `[context7-account-broker] ${text}` }],
    isError: true,
  };
}

function nextUtcDay(now = new Date()): number {
  return Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
}

function nextQuotaRefresh(quota: Quota): number {
  return quota.blocked ? Math.min(quota.resetAt, nextUtcDay()) : quota.resetAt;
}

function affinitySubject(tool: string, args: Record<string, unknown> | undefined): string {
  const value =
    tool === "query-docs"
      ? args?.libraryId
      : tool === "resolve-library-id"
        ? args?.libraryName
        : undefined;
  return typeof value === "string" ? `library:${value.toLowerCase()}` : `tool:${tool}`;
}

function resolvedLibraries(result: CallToolResult): string[] {
  return [...resultText(result).matchAll(/Context7-compatible library ID:\s*(\/\S+)/g)].map(
    ([, libraryId]) => `library:${libraryId?.toLowerCase()}`
  );
}

export async function probeQuota(apiKey: string): Promise<Quota> {
  const url = new URL("https://context7.com/api/v2/libs/search");
  url.searchParams.set("libraryName", "Context7");
  url.searchParams.set("query", "quota status check");
  const response = await fetch(url, { headers: { Authorization: `Bearer ${apiKey}` } });

  if (response.status !== 200 && response.status !== 429) {
    const body = await response.text();
    throw new Error(`quota check returned HTTP ${response.status}${body ? `: ${body}` : ""}`);
  }

  const header = (name: string): number => {
    const value = response.headers.get(name);
    if (value === null) throw new Error(`quota check omitted ${name}`);
    return Number(value);
  };
  const limit = header("RateLimit-Limit");
  const remaining = header("RateLimit-Remaining");
  const resetAt = header("RateLimit-Reset") * 1000;
  await response.body?.cancel();
  if (
    ![limit, remaining, resetAt].every(Number.isInteger) ||
    limit <= 0 ||
    remaining > limit ||
    resetAt <= 0
  ) {
    throw new Error("quota check returned invalid rate-limit headers");
  }

  return {
    limit,
    remaining,
    resetAt,
    updatedAt: Date.now(),
    blocked: response.status === 429,
  };
}

export function quotaSummary({ limit, remaining, resetAt, blocked }: Quota): string {
  const used = limit - remaining;
  const percentage = Math.round((used / limit) * 100);
  return `${used}/${limit} used (${percentage}%)${blocked ? ", blocked" : ""}; resets ${new Date(resetAt).toISOString()}`;
}

export class AccountPool {
  readonly tools: readonly Tool[];
  private readonly accounts: readonly AccountState[];
  private readonly toolNames: ReadonlySet<string>;
  private readonly inFlight = new Map<string, Promise<CallToolResult>>();
  private nextAccount = 0;

  constructor(
    accounts: readonly Account[],
    private readonly cooldownMs: number,
    private readonly cache: ResultCache,
    private readonly scope: string
  ) {
    const [first, ...rest] = accounts;
    if (!first) throw new Error("No configured Context7 account could be connected");
    for (const account of rest) {
      if (!isDeepStrictEqual(account.tools, first.tools)) {
        throw new Error(`${account.id} advertised tools that differ from ${first.id}`);
      }
    }

    this.tools = first.tools;
    this.toolNames = new Set(first.tools.map(({ name }) => name));
    this.accounts = accounts.map((account, index) => ({
      ...account,
      index,
      cooldownUntil: 0,
      quotaRefreshAt: nextQuotaRefresh(account.quota),
    }));
  }

  private refreshQuota(account: AccountState): Promise<void> {
    if (!account.refreshing) {
      account.refreshing = account
        .refreshQuota()
        .then((quota) => {
          account.quota = quota;
          account.quotaRefreshAt = nextQuotaRefresh(quota);
          account.cooldownUntil = 0;
          console.error(`[context7-account-broker] ${account.id} quota: ${quotaSummary(quota)}`);
        })
        .catch((error) => {
          account.quotaRefreshAt = Date.now() + this.cooldownMs;
          console.error(
            `[context7-account-broker] Failed to refresh ${account.id} quota: ${message(error)}`
          );
        })
        .finally(() => {
          account.refreshing = undefined;
        });
    }
    return account.refreshing;
  }

  callTool(name: string, args: Record<string, unknown> | undefined): Promise<CallToolResult> {
    const requestKey = this.cache.requestKey(this.scope, name, args);
    const running = this.inFlight.get(requestKey);
    if (running) return running;

    const request = this.callToolOnce(name, args, requestKey).finally(() => {
      this.inFlight.delete(requestKey);
    });
    this.inFlight.set(requestKey, request);
    return request;
  }

  private async callToolOnce(
    name: string,
    args: Record<string, unknown> | undefined,
    requestKey: string
  ): Promise<CallToolResult> {
    if (!this.toolNames.has(name)) {
      throw new ProtocolError(ProtocolErrorCode.InvalidParams, `Tool ${name} not found`);
    }

    const subject = affinitySubject(name, args);
    const assignedKey = await this.cache.getAffinity(this.scope, subject);
    const assigned = this.accounts.find(({ accountKey }) => accountKey === assignedKey);
    const cacheOrder = assigned
      ? [assigned, ...this.accounts.filter((account) => account !== assigned)]
      : this.accounts;
    for (const account of cacheOrder) {
      const cached = await this.cache.getResult(requestKey, account.accountKey);
      if (cached) {
        console.error(`[context7-account-broker] Cache hit for ${name}`);
        if (!assigned) await this.cache.setAffinity(this.scope, subject, account.accountKey);
        return cached;
      }
    }

    const now = Date.now();
    await Promise.all(
      this.accounts
        .filter(({ quotaRefreshAt }) => quotaRefreshAt <= now)
        .map((account) => this.refreshQuota(account))
    );

    const distance = (index: number): number =>
      (index - this.nextAccount + this.accounts.length) % this.accounts.length;
    const candidates = this.accounts
      .filter((account) => !account.quota.blocked && account.cooldownUntil <= Date.now())
      .sort(
        (a, b) =>
          (a === assigned ? -1 : b === assigned ? 1 : 0) ||
          (a.quota.limit - a.quota.remaining) / a.quota.limit -
            (b.quota.limit - b.quota.remaining) / b.quota.limit ||
          distance(a.index) - distance(b.index)
      );

    let lastFailure: string | undefined;
    for (const account of candidates) {
      this.nextAccount = (account.index + 1) % this.accounts.length;
      account.quota.remaining -= 1;
      account.quota.updatedAt = Date.now();

      try {
        const result = await account.client.callTool({ name, arguments: args });
        const text = resultText(result).trim();
        if (!RETRYABLE_RESULT.test(text)) {
          if (!result.isError) {
            const affinities = [subject, ...(name === "resolve-library-id" ? resolvedLibraries(result) : [])];
            await Promise.all([
              this.cache.setResult(requestKey, account.accountKey, result),
              ...affinities.map((key) =>
                this.cache.setAffinity(this.scope, key, account.accountKey)
              ),
            ]).catch((error) =>
              console.warn(`[context7-account-broker] Failed to update cache: ${message(error)}`)
            );
          }
          return result;
        }

        lastFailure = text || "retryable upstream failure";
        if (QUOTA_RESULT.test(text)) {
          account.quota.blocked = true;
          account.quota.remaining = 0;
          account.quotaRefreshAt = nextQuotaRefresh(account.quota);
        } else {
          account.cooldownUntil = Date.now() + this.cooldownMs;
        }
      } catch (error) {
        if (!isRetryableError(error)) {
          console.error(`[context7-account-broker] ${account.id} failed: ${message(error)}`);
          throw error;
        }
        lastFailure = message(error);
        account.cooldownUntil = Date.now() + this.cooldownMs;
      }

      console.warn(`[context7-account-broker] ${account.id} unavailable: ${lastFailure}`);
    }

    if (lastFailure !== undefined) {
      console.error(`[context7-account-broker] All accounts failed for ${name}: ${lastFailure}`);
      return brokerError(`All configured Context7 accounts failed for ${name}.`);
    }

    const retryAt = Math.min(
      ...this.accounts.map((account) =>
        account.quota.blocked ? account.quotaRefreshAt : account.cooldownUntil
      )
    );
    const retryDate = new Date(retryAt).toISOString();
    console.warn(`[context7-account-broker] All accounts unavailable until ${retryDate}`);
    return brokerError(`All configured Context7 accounts are unavailable. Retry after ${retryDate}.`);
  }

  async close(): Promise<void> {
    const results = await Promise.allSettled(this.accounts.map(({ client }) => client.close()));
    results.forEach((result, index) => {
      if (result.status === "rejected") {
        console.error(
          `[context7-account-broker] Failed to close ${this.accounts[index]?.id}: ${message(result.reason)}`
        );
      }
    });
  }
}
