# context7-account-broker

A Context7 MCP server that shares accounts, quota routing, caching, affinity, and in-flight requests across projects and coding agents. Use its native stdio command with any MCP client, or its OpenCode plugin for automatic registration.

> [!IMPORTANT]
> This package supports Linux x86-64 only. macOS, Windows, Linux ARM64, and other platforms are not supported.

## Setup

Add at least one Context7 account:

```sh
context7-account-broker accounts add personal
```

The command reads the API key from a hidden terminal prompt and stores it in a private `accounts.json` file.

### Any MCP client

Configure a local stdio server with command `context7-account-broker` and arguments `["mcp"]`. For clients that use `mcpServers`:

```json
{
  "mcpServers": {
    "context7-broker": {
      "command": "context7-account-broker",
      "args": ["mcp"]
    }
  }
}
```

Codex uses the same command:

```toml
[mcp_servers.context7-broker]
command = "context7-account-broker"
args = ["mcp"]
tool_timeout_sec = 30
```

The command starts or reuses the shared broker. It speaks MCP on stdin/stdout; diagnostics go to stderr. Accounts and server tokens stay outside the client's configuration.

### OpenCode

Add the plugin to `opencode.json`:

```json
{
  "plugin": ["@mirsella/context7-account-broker"]
}
```

That is the complete OpenCode configuration. Do not add an `mcp.context7-broker` entry: the plugin creates it and refuses to replace an existing entry.

## Lifecycle

Each `mcp` process starts or reuses a healthy broker for its package version, then connects to its authenticated loopback HTTP endpoint. The native command handles authentication internally. The OpenCode plugin registers this same stdio command.

Clients using the same version and XDG configuration/cache directories share the broker and its state. Different versions use separate ports, tokens, and caches. Disconnecting a client closes its stdio transport; the shared broker remains available. A containing process manager may stop it with the harness. No separate service setup is needed.

The `start` command remains available for direct HTTP clients and prints connection JSON. Keep that output private: it includes the broker token.
Startup makes no Context7 requests. Context7 is contacted only for tool calls and the explicit `status` command.

## Commands

```text
context7-account-broker mcp
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

The npm package contains a static `x86_64-unknown-linux-musl` binary. Each active version runs one current-thread Tokio broker exposing authenticated Streamable HTTP MCP on an OS-assigned loopback port. Each client runs a stdio process backed by the Rust MCP SDK. Requests can overlap; the shared broker limits upstream Context7 REST API calls to four at a time.

The broker provides the canonical `resolve-library-id` and `query-docs` tools. It routes toward account affinity and lower quota use, rotates unknown quota fairly, fails over account-specific `401`, `402`, `403`, and `429` responses, and caps shared endpoint failures at two attempts. Successful results use an account-partitioned disk cache. Identical typed requests share one cancellation-safe in-flight producer.

Default private state:

- Accounts: `${XDG_CONFIG_HOME:-$HOME/.config}/context7-account-broker/accounts.json`
- Broker state: `${XDG_CONFIG_HOME:-$HOME/.config}/context7-account-broker/<version>/broker.json`
- Cache and log: `${XDG_CACHE_HOME:-$HOME/.cache}/context7-account-broker/<version>/`

## Development and Nix

The Rust source owns both transports, startup, account-file handling, and tests. `nix/package.nix` builds the native command from this repository's `Cargo.lock` and wraps its CA bundle. Consumers can call that expression with their own Nixpkgs; no external source patches or transport wrappers are needed.

```sh
cargo test --locked
bun test tests/plugin.test.ts
```

## License

MIT
