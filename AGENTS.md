# Agent Notes

## Deployment

- The release workflow tests and packages the static Rust binary, creates the GitHub release, and publishes the npm package.
- Do not commit credentials, server tokens, or API keys.
- The native MCP command starts and configures the shared broker; the OpenCode plugin registers that command. Do not add a separate service-manager setup.
