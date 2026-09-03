# context7-account-broker

An OpenCode plugin that runs one local Context7 MCP broker across projects and shares quota, caching, affinity, and in-flight requests across configured API keys.

> [!IMPORTANT]
> This package supports Linux x86-64 only. macOS, Windows, Linux ARM64, and other platforms are not supported.

## Setup

Add at least one Context7 account:

```sh
bunx @mirsella/context7-account-broker accounts add personal
```

The command reads the API key from a hidden terminal prompt and stores it in a private `accounts.json` file.

Then add the plugin to `opencode.json`:

```json
{
  "plugin": ["@mirsella/context7-account-broker"]
}
```

That is the complete OpenCode configuration. Do not add an `mcp.context7-broker` entry: the plugin creates it and refuses to replace an existing entry.

## Lifecycle

When OpenCode loads the plugin, the packaged Linux x64 binary runs `start`. It creates a private server token if needed, reuses an authenticated healthy broker on `127.0.0.1:14197`, or launches the shared broker and waits briefly for readiness. The plugin reads the token into OpenCode's in-memory MCP configuration; it never writes the token to `opencode.json` or passes it in process arguments.

The broker stays available across project and session lifetimes. Its containing process manager may stop it with OpenCode; a later plugin launch reuses it when healthy or starts a replacement when it is absent. There is no supervisor, heartbeat, separate systemd unit, PATH installation, manual token, or last-client shutdown.

Startup makes no Context7 requests. Context7 is contacted only for tool calls and the explicit `status` command.

## Commands

```text
context7-account-broker start
context7-account-broker serve
context7-account-broker accounts list
context7-account-broker accounts add NAME
context7-account-broker accounts remove NAME
context7-account-broker status
context7-account-broker config
```

Use `bunx @mirsella/context7-account-broker ...` when the package is not otherwise installed.

### Check account quotas

```sh
bunx @mirsella/context7-account-broker status
```

The command prints each account's used and total requests, percentage used, blocked state, and UTC reset time. It makes one Context7 request per configured account to read the current quota headers; the broker does not probe quotas in the background.

## Architecture

The plugin contains a static `x86_64-unknown-linux-musl` binary. One current-thread Tokio process exposes authenticated Streamable HTTP MCP on loopback. It forwards requests directly to the official Context7 REST API with bounded concurrency four.

The broker provides the canonical `resolve-library-id` and `query-docs` tools. It routes toward account affinity and lower quota use, rotates unknown quota fairly, fails over account-specific `401`, `403`, and `429` responses, and caps shared endpoint failures at two attempts. Successful results use an account-partitioned disk cache. Identical typed requests share one cancellation-safe in-flight producer.

Default private state:

- Accounts: `$XDG_CONFIG_HOME/context7-account-broker/accounts.json`
- Server token: `$XDG_CONFIG_HOME/context7-account-broker/server-token`
- Cache and launch log: `$XDG_CACHE_HOME/context7-account-broker/`

Optional environment settings are `CONTEXT7_BROKER_CONFIG`, `CONTEXT7_API_KEY`, `CONTEXT7_API_KEYS`, `CONTEXT7_BROKER_INCLUDE_ENV`, `CONTEXT7_BROKER_TOKEN_FILE`, `CONTEXT7_CACHE_DIR`, `CONTEXT7_CACHE_TTL_DAYS`, `CONTEXT7_ACCOUNT_COOLDOWN_MS`, and `CONTEXT7_BROKER_PORT`.

## License

MIT
