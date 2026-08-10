import { randomUUID } from "node:crypto";
import { chmod, mkdir, readFile, rename, unlink, writeFile } from "node:fs/promises";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

export const APP_NAME = "context7-account-broker";

interface StoredAccount {
  name: string;
  apiKey: string;
}

export interface Credential extends StoredAccount {
  source: "config" | "environment";
}

interface AccountsFile {
  version: 1;
  accounts: StoredAccount[];
}

const ACCOUNT_NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$/;

export function accountsPath(env: NodeJS.ProcessEnv = process.env): string {
  return (
    env.CONTEXT7_BROKER_CONFIG ??
    join(env.XDG_CONFIG_HOME ?? join(homedir(), ".config"), APP_NAME, "accounts.json")
  );
}

export function cachePath(env: NodeJS.ProcessEnv = process.env): string {
  return (
    env.CONTEXT7_CACHE_DIR ??
    join(env.XDG_CACHE_HOME ?? join(homedir(), ".cache"), APP_NAME)
  );
}

export async function loadConfiguredAccounts(
  env: NodeJS.ProcessEnv = process.env
): Promise<StoredAccount[]> {
  return (await readAccountsFile(env)).accounts;
}

export async function loadAccounts(env: NodeJS.ProcessEnv = process.env): Promise<Credential[]> {
  const configured = await loadConfiguredAccounts(env);
  const credentials: Credential[] = configured.map((account) => ({
    ...account,
    source: "config",
  }));
  if (configured.length > 0 && env.CONTEXT7_BROKER_INCLUDE_ENV !== "1") {
    return credentials;
  }

  const knownKeys = new Set(configured.map(({ apiKey }) => apiKey));
  const knownNames = new Set(configured.map(({ name }) => name));
  const environmentKeys = [env.CONTEXT7_API_KEYS, env.CONTEXT7_API_KEY]
    .flatMap((value) => value?.split(/[\s,]+/) ?? [])
    .filter(Boolean);

  let environmentIndex = 1;
  for (const apiKey of new Set(environmentKeys)) {
    if (knownKeys.has(apiKey)) continue;
    while (knownNames.has(`env-${environmentIndex}`)) environmentIndex += 1;
    const name = `env-${environmentIndex++}`;
    credentials.push({ name, apiKey, source: "environment" });
    knownNames.add(name);
  }
  return credentials;
}

export async function addAccount(
  name: string,
  apiKey: string,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  validateName(name);
  if (!apiKey.startsWith("ctx7sk")) {
    throw new Error("Context7 API keys must start with ctx7sk");
  }

  const file = await readAccountsFile(env);
  if (file.accounts.some((account) => account.name === name)) {
    throw new Error(`Account ${name} already exists`);
  }
  if (file.accounts.some((account) => account.apiKey === apiKey)) {
    throw new Error("That API key is already configured");
  }
  file.accounts.push({ name, apiKey });
  await writeAccountsFile(file, env);
}

export async function removeAccount(
  name: string,
  env: NodeJS.ProcessEnv = process.env
): Promise<void> {
  const file = await readAccountsFile(env);
  const accounts = file.accounts.filter((account) => account.name !== name);
  if (accounts.length === file.accounts.length) {
    throw new Error(`Configured account ${name} does not exist`);
  }
  await writeAccountsFile({ ...file, accounts }, env);
}

function validateName(name: string): void {
  if (!ACCOUNT_NAME.test(name)) {
    throw new Error("Account names must use 1-64 letters, numbers, dots, underscores, or hyphens");
  }
}

async function readAccountsFile(env: NodeJS.ProcessEnv): Promise<AccountsFile> {
  const path = accountsPath(env);
  let value: unknown;
  try {
    value = JSON.parse(await readFile(path, "utf8"));
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return { version: 1, accounts: [] };
    throw new Error(`Cannot read ${path}: ${String(error)}`);
  }

  if (!isAccountsFile(value)) throw new Error(`Invalid account configuration in ${path}`);
  return value;
}

function isAccountsFile(value: unknown): value is AccountsFile {
  if (!value || typeof value !== "object") return false;
  const file = value as Partial<AccountsFile>;
  return (
    file.version === 1 &&
    Array.isArray(file.accounts) &&
    file.accounts.every(
      (account) =>
        account &&
        typeof account.name === "string" &&
        ACCOUNT_NAME.test(account.name) &&
        typeof account.apiKey === "string" &&
        account.apiKey.startsWith("ctx7sk")
    ) &&
    new Set(file.accounts.map(({ name }) => name)).size === file.accounts.length &&
    new Set(file.accounts.map(({ apiKey }) => apiKey)).size === file.accounts.length
  );
}

async function writeAccountsFile(file: AccountsFile, env: NodeJS.ProcessEnv): Promise<void> {
  const path = accountsPath(env);
  await mkdir(dirname(path), { recursive: true, mode: 0o700 });
  const temporary = `${path}.${process.pid}.${randomUUID()}.tmp`;
  try {
    await writeFile(temporary, `${JSON.stringify(file, null, 2)}\n`, { mode: 0o600 });
    await rename(temporary, path);
    await chmod(path, 0o600);
  } finally {
    await unlink(temporary).catch(() => undefined);
  }
}
