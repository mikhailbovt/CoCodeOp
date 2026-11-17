# Compatibility

| Component | Supported profile |
| --- | --- |
| Core | Linux amd64, PostgreSQL 18.7, authenticated Streamable HTTP MCP |
| CLI | Linux amd64 and Windows amd64 release binaries |
| Lead clients | Codex, Claude Code and compatible MCP clients such as OpenCode |
| Runner and checkout coordinator | Linux amd64, rootless Podman, cgroup v2 and a local Linux filesystem |
| Managed workers | Codex 0.148.0, OpenCode 1.19.5, or an approved `generic-jsonl.v1` adapter |
| Shared editing | One authoritative checkout with mediated reads, writes and commands |

The release was exercised on two Debian 13 VMs with independent lead identities and native Codex and OpenCode workers. The Codex plugin called the MCP bootstrap tool. Claude Code confirmed its MCP connection. Authenticated model turns with Claude and Gemini require provider credentials.

Codex workers use a Responses-compatible endpoint. OpenCode workers use a Chat Completions-compatible endpoint. Local Ollama was used for full collaboration testing. FreeLLMAPI v0.16.30 completed short managed turns but returned `502 empty_completion` on a longer turn. [Runner profiles](runners.md#other-models-and-clients)

Clients can connect remotely over HTTPS or through a local SSH tunnel. On Windows, keep the lead in the desktop app and run CoCodeOp's Linux runner in a local VM. [Workspace setup](getting-started.md)
