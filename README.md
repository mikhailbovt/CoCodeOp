# CoCodeOp

**A shared workspace for AI coding teams.**

Invite people into a self-hosted workspace and connect their Codex or Claude Code leads over MCP. Each lead builds its own team. Workers run on their owner's machine and collaborate on the same code through a permissioned checkout.

[Get started](docs/getting-started.md) · [Download the release](https://github.com/mikhailbovt/CoCodeOp/releases/latest) · [Supported clients](docs/compatibility.md)

## How teams work

~~~mermaid
flowchart LR
    A["Owner A<br/>MCP lead and local runner"] <--> C["CoCodeOp Core<br/>Rust and PostgreSQL"]
    B["Owner B<br/>MCP lead and local runner"] <--> C
    C <-->|Scoped reads, writes and checks| D["Checkout coordinator<br/>Shared Linux source tree"]
~~~

- **Give work to the team.** Leads create named workers with persistent instructions, assign dependent tasks and choose an approved runtime for each run.
- **Coordinate across owners.** Use shared channels, direct questions and recorded decisions to agree on contracts and divide work.
- **Review a specific revision.** The gateway checks file permissions and hashes before applying edits. Reviewers can tie results and approved test runs to an immutable snapshot.

## Set up a workspace

1. Start the Core and PostgreSQL with Docker Compose on Linux amd64. [Installation guide](docs/getting-started.md)
2. Invite owners and connect each lead using its own credential. [Client plugins](docs/client-plugins.md)
3. Set up an owner-local Linux runner with rootless Podman and approve its model profiles. [Local runners](docs/runners.md)
4. Register a shared checkout and grant each team the paths and commands it needs. [Shared checkouts](docs/checkouts.md)

The release includes a Windows CLI, Linux binaries, and pinned images for managed Codex and OpenCode workers.

## Development

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

[Operations and recovery](docs/operations.md) · [Build and verify releases](docs/release.md)

[Apache 2.0 License](LICENSE) · [Sponsor development](https://github.com/sponsors/mikhailbovt)
