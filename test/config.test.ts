import { mkdtemp, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import {
  addAccount,
  loadAccounts,
  loadConfiguredAccounts,
  removeAccount,
} from "../src/config.js";

describe("account configuration", () => {
  let directory: string;
  let env: NodeJS.ProcessEnv;

  beforeEach(async () => {
    directory = await mkdtemp(join(tmpdir(), "context7-account-config-"));
    env = { CONTEXT7_BROKER_CONFIG: join(directory, "accounts.json") };
  });

  afterEach(async () => {
    await rm(directory, { recursive: true, force: true });
  });

  it("persists named accounts with private permissions", async () => {
    await addAccount("personal", "ctx7sk-configured", env);

    expect(await loadAccounts(env)).toEqual([
      { name: "personal", apiKey: "ctx7sk-configured", source: "config" },
    ]);
    expect(await loadConfiguredAccounts(env)).toEqual([
      { name: "personal", apiKey: "ctx7sk-configured" },
    ]);
    expect((await stat(env.CONTEXT7_BROKER_CONFIG!)).mode & 0o777).toBe(0o600);
  });

  it("uses configured accounts as an allowlist", async () => {
    await addAccount("personal", "ctx7sk-configured", env);
    env.CONTEXT7_API_KEYS = "ctx7sk-configured,ctx7sk-environment";

    expect(await loadAccounts(env)).toEqual([
      { name: "personal", apiKey: "ctx7sk-configured", source: "config" },
    ]);
  });

  it("can explicitly include environment keys", async () => {
    await addAccount("personal", "ctx7sk-configured", env);
    env.CONTEXT7_API_KEYS = "ctx7sk-configured,ctx7sk-environment";
    env.CONTEXT7_BROKER_INCLUDE_ENV = "1";

    expect(await loadAccounts(env)).toEqual([
      { name: "personal", apiKey: "ctx7sk-configured", source: "config" },
      { name: "env-1", apiKey: "ctx7sk-environment", source: "environment" },
    ]);
  });

  it("removes configured accounts by name", async () => {
    await addAccount("personal", "ctx7sk-configured", env);
    await removeAccount("personal", env);

    expect(await loadAccounts(env)).toEqual([]);
  });
});
