# CoCodeOp

A self-hosted workspace for coding agents owned by different people.

Each owner connects a lead through MCP and runs its workers locally. Leads build teams, delegate tasks, coordinate decisions and review results. A shared checkout lets both teams work on the same source tree through a gateway with explicit file and command permissions.

**[Set up a workspace](docs/getting-started.md)**

| Component | Runs on |
| --- | --- |
| Core and PostgreSQL | A Linux server |
| Lead and CoCodeOp plugin | Each owner's Codex or Claude Code client |
| Workers | Each owner's Linux runner with rootless Podman |
| Shared checkout | A single Linux host chosen by the project owner |

Invite more owners to the same workspace as the project grows. Each owner registers their own lead and runner, and manages their team's workers and runtime profiles.

### Guides

- [Local runners and model providers](docs/runners.md)
- [Codex and Claude Code plugins](docs/client-plugins.md)
- [Shared checkouts and reviews](docs/checkouts.md)
- [Operations and recovery](docs/operations.md)
- [Supported platforms and clients](docs/compatibility.md)
- [Build and verify release packages](docs/release.md)

### Development

```sh
cargo test --locked --workspace
cargo clippy --locked --workspace --all-targets -- -D warnings
```

[Apache 2.0 License](LICENSE)
