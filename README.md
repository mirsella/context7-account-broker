# Context7 Account Broker

Drop-in stdio MCP proxy that balances Context7 requests across API keys you are
authorized to use. It forwards Context7's current tool definitions and results
without translation.

The broker reads Context7's official `RateLimit-Limit`, `RateLimit-Remaining`,
and `RateLimit-Reset` response headers when it starts. It tracks subsequent
calls locally and assigns each library in each OpenCode project to the account
with the lowest proportional usage. Later queries for that library prefer the
same account, with temporary failover when it is unavailable. Exhausted
accounts are rechecked at their reset time and once per UTC day for Context7's
free-plan daily bonus calls.

Successful tool results are cached by exact arguments for 30 days. The cache is
persistent across broker restarts, coalesces concurrent identical requests,
and is partitioned by project and API-key hash so removed credentials cannot
serve their cached private content. Set `CONTEXT7_CACHE_TTL_DAYS=60` for a
60-day TTL. Context7 recommends caching for hours or days; longer values trade
freshness for fewer requests.

Each startup and quota refresh consumes one Context7 API request per checked
account because Context7 applies quota accounting to every API request,
including validation failures, `HEAD`, and `OPTIONS` requests.

Use only API keys for accounts you own or are authorized to aggregate, and
follow Context7's terms and plan limits. The broker does not create accounts or
hide failures.

## OpenCode

Set a comma- or newline-separated list of keys in the environment that starts
OpenCode:

```bash
export CONTEXT7_API_KEYS='ctx7sk_first,ctx7sk_second,ctx7sk_third'
```

Configure the MCP server in `~/.config/opencode/opencode.jsonc`:

```jsonc
"context7": {
  "type": "local",
  "command": ["npx", "-y", "git+https://github.com/mirsella/context7-account-broker.git"],
  "environment": { "npm_config_allow_git": "all" },
  "timeout": 30000,
  "enabled": true
}
```

OpenCode continues to see the standard Context7 tool names and schemas.

## Account CLI

The package installs a `context7-account-broker` binary. With the local package,
replace the command below with `npx -y /home/mirsella/dev/context7-account-broker`.

```bash
# Add a named account using a hidden API-key prompt
npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git accounts add personal

# Non-interactive input for scripts
printf '%s\n' "$CONTEXT7_API_KEY" |
  npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git accounts add personal

# Show names, credential sources, and non-secret fingerprints
npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git accounts list

# Fetch authoritative quota usage and reset dates
npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git status

# Remove a file-configured account
npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git accounts remove personal

# Show effective configuration and storage paths
npm_config_allow_git=all npx -y git+https://github.com/mirsella/context7-account-broker.git config
```

`status` consumes one Context7 request per account because Context7 attaches
quota state to API responses rather than exposing a free status endpoint.
Configured credentials are stored in
`~/.config/context7-account-broker/accounts.json` with mode `0600`. When that
file contains at least one account, it is an allowlist and inherited environment
keys are ignored. Set `CONTEXT7_BROKER_INCLUDE_ENV=1` to explicitly include
environment keys; duplicate keys are ignored.

## Local Development

Requirements: Node.js 20 or newer and pnpm.

```bash
pnpm install
pnpm build
```

Before the package is published, test the same `npx` installation path with:

```jsonc
"command": ["npx", "-y", "/home/mirsella/dev/context7-account-broker"]
```

`CONTEXT7_API_KEY` is accepted for a single-key setup. Optional settings:

- `CONTEXT7_MCP_URL` changes the upstream MCP endpoint.
- `CONTEXT7_ACCOUNT_COOLDOWN_MS` controls how long transiently failed accounts
  are skipped; it defaults to `30000`.
- `CONTEXT7_CACHE_TTL_DAYS` controls result and affinity expiry; it defaults to
  `30`.
- `CONTEXT7_CACHE_DIR` changes the cache directory. The default follows
  `XDG_CACHE_HOME`, falling back to `~/.cache/context7-account-broker`.
- `CONTEXT7_PROJECT_ID` overrides the current working directory used to scope
  cache and affinity entries.
- `CONTEXT7_BROKER_INCLUDE_ENV=1` adds `CONTEXT7_API_KEY(S)` to configured file
  accounts; it is disabled by default when the account file is non-empty.

Delete the cache directory to invalidate all cached results immediately.

## Verification

```bash
pnpm typecheck
pnpm test
pnpm build
```
