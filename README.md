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

When OpenCode loads the plugin, the packaged Linux x64 binary runs `start`. It creates private runtime state for that package version, reuses its matching healthy broker, or launches one on an available loopback port. Each broker process gets a fresh token, which `start` returns directly to the plugin for OpenCode's in-memory MCP configuration. The token is never written to `opencode.json` or passed in process arguments.

OpenCode processes using the same plugin version share one broker. Separate OpenCode processes can use different plugin versions at the same time because each version has its own port, token, cache, and broker process. A broker stays available across project and session lifetimes; its containing process manager may stop it with OpenCode. There is no supervisor, heartbeat, separate systemd unit, PATH installation, manual token, or last-client shutdown.

Startup makes no Context7 requests. Context7 is contacted only for tool calls and the explicit `status` command.

## Commands

```text
context7-account-broker start
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

The plugin contains a static `x86_64-unknown-linux-musl` binary. Each active package version runs one current-thread Tokio process exposing authenticated Streamable HTTP MCP on an OS-assigned loopback port. It forwards requests directly to the official Context7 REST API with bounded concurrency four.

The broker provides the canonical `resolve-library-id` and `query-docs` tools. It routes toward account affinity and lower quota use, rotates unknown quota fairly, fails over account-specific `401`, `402`, `403`, and `429` responses, and caps shared endpoint failures at two attempts. Successful results use an account-partitioned disk cache. Identical typed requests share one cancellation-safe in-flight producer.

Default private state:

- Accounts: `${XDG_CONFIG_HOME:-$HOME/.config}/context7-account-broker/accounts.json`
- Broker state: `${XDG_CONFIG_HOME:-$HOME/.config}/context7-account-broker/<version>/broker.json`
- Cache and log: `${XDG_CACHE_HOME:-$HOME/.cache}/context7-account-broker/<version>/`

## License

MIT
