# Context7 Account Broker

[![CI](https://github.com/mirsella/context7-account-broker/actions/workflows/ci.yml/badge.svg)](https://github.com/mirsella/context7-account-broker/actions/workflows/ci.yml)
[![Node.js 20+](https://img.shields.io/badge/Node.js-20%2B-339933?logo=node.js&logoColor=white)](https://nodejs.org/)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

A drop-in Context7 MCP proxy for people who use more than one authorized API
key. It balances requests by quota usage, keeps each library on the same account,
and caches successful responses on disk.

OpenCode sees the standard Context7 tools:

- `resolve-library-id`
- `query-docs`

The broker handles account selection, failover, caching, and quota refreshes
without changing tool schemas or results.

## What it does

- Routes new work to the account with the lowest proportional quota usage.
- Keeps a library on the same account within each project.
- Fails over when an account is blocked, rate limited, unauthorized, or
  temporarily unreachable.
- Reads Context7's `RateLimit-*` headers instead of estimating quota locally.
- Caches exact successful calls for 30 days by default.
- Coalesces concurrent identical requests into one upstream call.
- Stores named credentials in a private `0600` configuration file.
- Provides CLI commands for account management, quota status, and diagnostics.

## Quick start

### 1. Add your Context7 accounts

The command prompts for the API key without displaying it:

```bash
npx -y @mirsella/context7-account-broker accounts add personal
```

Repeat the command with a different name for each account. To verify the saved
accounts without exposing their keys:

```bash
npx -y @mirsella/context7-account-broker accounts list
```

### 2. Configure OpenCode

Add the server to `~/.config/opencode/opencode.jsonc`:

```jsonc
{
  "mcp": {
    "context7": {
      "type": "local",
      "command": ["npx", "-y", "@mirsella/context7-account-broker"],
      "timeout": 30000,
      "enabled": true
    }
  }
}
```

Restart OpenCode, then check the connection:

```bash
opencode mcp list
```

The package is published to npm, so no Git checkout or local build is required.

## CLI

The examples below use the published npm package.

| Command | Purpose |
| --- | --- |
| `accounts add [name]` | Add a named API key using a hidden prompt or stdin |
| `accounts list` | List account names, sources, and key fingerprints |
| `accounts remove <name>` | Remove a file-configured account |
| `status` | Fetch current quota usage and reset times |
| `config` | Print effective paths and runtime settings |
| `help` | Show command help |
| `serve` | Start the stdio MCP server (the default) |

For example:

```bash
BROKER=@mirsella/context7-account-broker

npx -y "$BROKER" status
npx -y "$BROKER" config
npx -y "$BROKER" accounts remove personal
```

For non-interactive account setup:

```bash
BROKER=@mirsella/context7-account-broker
printf '%s\n' "$CONTEXT7_API_KEY" |
  npx -y "$BROKER" accounts add personal
```

## Account selection

At startup, the broker probes each key and reads Context7's authoritative
`RateLimit-Limit`, `RateLimit-Remaining`, and `RateLimit-Reset` headers.

For a library without an existing assignment, it chooses the available account
with the lowest proportional usage:

```text
used / limit = (limit - remaining) / limit
```

Equal accounts rotate in round-robin order. Once selected, the account is saved
as the preferred account for that library and project. A temporary failure can
move one request elsewhere without discarding the preference.

Blocked accounts are checked again at their reset time. Free-plan accounts are
also checked at the next UTC day so Context7's daily bonus calls can become
available.

## Cache

Successful tool results are cached by:

- project scope
- tool name
- canonicalized arguments
- API-key hash

The API-key partition prevents a removed account from serving its cached private
content. Expired files are removed on startup and lazily when read. Concurrent
identical calls share one in-flight request.

The default cache directory is:

```text
~/.cache/context7-account-broker
```

Delete that directory to invalidate all cached results and affinities.

## Credentials

Named accounts are stored at:

```text
~/.config/context7-account-broker/accounts.json
```

The directory uses mode `0700` and the file uses mode `0600`.

When this file contains accounts, it acts as an allowlist. Inherited
`CONTEXT7_API_KEY` and `CONTEXT7_API_KEYS` values are ignored unless
`CONTEXT7_BROKER_INCLUDE_ENV=1` is set. This prevents an unrelated shell key from
silently joining a configured pool.

With no configured account file, the broker accepts:

```bash
export CONTEXT7_API_KEYS='ctx7sk_first,ctx7sk_second'
```

## Configuration

| Environment variable | Default | Description |
| --- | --- | --- |
| `CONTEXT7_API_KEY` | unset | One API key when no account file is configured |
| `CONTEXT7_API_KEYS` | unset | Separated API keys |
| `CONTEXT7_BROKER_CONFIG` | XDG config path | Override the accounts file path |
| `CONTEXT7_BROKER_INCLUDE_ENV` | unset | Include environment keys with `1` |
| `CONTEXT7_MCP_URL` | `https://mcp.context7.com/mcp` | Upstream MCP endpoint |
| `CONTEXT7_ACCOUNT_COOLDOWN_MS` | `30000` | Transient failure cooldown |
| `CONTEXT7_CACHE_TTL_DAYS` | `30` | Result and affinity cache lifetime |
| `CONTEXT7_CACHE_DIR` | XDG cache path | Override the cache directory |
| `CONTEXT7_PROJECT_ID` | current directory | Cache and affinity scope |

## Quota cost

Context7 attaches quota state to normal API responses rather than exposing a
free status endpoint. Each startup probe, refresh, and `status` check consumes
one API request per checked account.

## Development

Requirements: Node.js 20 or newer and pnpm.

```bash
git clone https://github.com/mirsella/context7-account-broker.git
cd context7-account-broker
pnpm install
pnpm typecheck
pnpm test
pnpm build
```

Run the local binary:

```bash
node dist/index.js accounts list
node dist/index.js status
node dist/index.js serve
```

## Responsible use

Use API keys only for accounts you own or are authorized to aggregate. Follow
Context7's terms and plan limits. The broker does not create accounts, bypass
authentication, or hide upstream failures.

## License

[MIT](LICENSE)
