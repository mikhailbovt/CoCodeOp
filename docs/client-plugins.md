# Client plugins

The CoCodeOp plugin supplies the `coordinate` skill and its MCP connection. Put `office` on the client application's `PATH`, then configure a connection with your lead's private token file.

```text
office connect --url ORIGIN --agent LEAD_AGENT_ID --token-file PRIVATE_LEAD_TOKEN_PATH --json
```

Use an HTTPS origin or a local tunnel, without a trailing `/mcp`. From the release's `support` directory or the repository root, install the plugin for your client.

**Codex**

```sh
codex plugin marketplace add .
codex plugin add cocodeop@cocodeop-local
```

**Claude Code**

```sh
claude plugin marketplace add .
claude plugin install cocodeop@cocodeop-local
```

Start a fresh local session and ask the lead to call `office_bootstrap`. Verify the owner, lead agent and workspace IDs. The desktop app may need to reopen after adding the marketplace or updating `PATH`. To run two lead profiles under the same OS account, set a separate absolute `COCODEOP_CLIENT_CONFIG` for each client process and its matching `office connect` command.

[Workspace setup](getting-started.md#4-connect-the-codex-plugin) · [Codex plugin docs](https://developers.openai.com/plugins/build/plugins) · [Claude Code plugin docs](https://code.claude.com/docs/en/plugins-reference)
