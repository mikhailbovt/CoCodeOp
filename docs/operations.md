# Operations

Use the [installation guide](getting-started.md) to start a package, enroll owners and connect their leads.

## Run from source

Run from the repository root with Docker Compose and Python 3.11 or newer. The server binds to loopback on the host and uses a private Compose network for PostgreSQL.

```bash
install -d -m 700 "$HOME/.config/cocodeop-core"
export COCODEOP_SECRETS_DIR="$HOME/.config/cocodeop-core/db"
if [ ! -d "$COCODEOP_SECRETS_DIR" ]; then
  python3 deploy/init-secrets.py "$COCODEOP_SECRETS_DIR"
fi
docker compose -p cocodeop -f deploy/compose.yaml up --build --wait
docker compose -p cocodeop -f deploy/compose.yaml exec -T server office status --json
```

For the bootstrap command, follow [Create a workspace and invite owners](getting-started.md#2-create-a-workspace-and-invite-owners). When running from source, use `docker compose -p cocodeop -f deploy/compose.yaml` in place of `core`. Keep the database volume and credentials directory across restarts. HTTP readiness reports the serving schema version.

## Inspect work

Run such commands using the relevant owner's or lead's credential. For `office doctor`, provide the lead token directly as a private file.

```sh
office status --url ORIGIN --json
office doctor --url ORIGIN --token-file /private/lead.token \
  --agent AGENT_ID --runner RUNNER_ID --json
office task explain --url ORIGIN --workspace WORKSPACE_ID --id TASK_ID --json
office runs logs --url ORIGIN --workspace WORKSPACE_ID --task TASK_ID --run RUN_ID --json
```

`doctor` checks the MCP handshake, the authenticated lead and the selected runner's advertised profile. Run `office-runner doctor` on the owner host to check its local image, state and sandbox. A queued job needs an active runner profile, a matching runtime and available budget. Coding jobs also need an active checkout coordinator and an acknowledged WorkScope.

`office_worker_status` returns the lead's worker profile, tasks and recent attempts. Each owner's provider key and worker logs remain on their own runner. Core records the coordination events, run IDs and execution evidence.

## Control execution

Use `office scheduler control` and `office scheduler set-control` to manage workspace, owner, team or agent limits. Each update includes the expected revision, a reason and a new idempotency key.

| Mode | Effect |
| --- | --- |
| `running` | Admits new work |
| `paused` | Holds new jobs while active attempts can finish |
| `stopped` | Revokes active execution rights and asks runners to stop |

A run with an uncertain exit stays in reconciliation until the original runner confirms what happened. Keep its state directory and do not manually requeue a publication with an unknown outcome. For a private operator export or workspace removal, see `python3 deploy/workspace.py --help`.

## Rotate access

- Keep human, lead and runner credentials in separate private files. Use the human token for setup, the lead token with `office connect`, and the runner token with `office-runner`.
- Issue replacement credentials with `office credential issue`. Verify the new credential before revoking the previous one with `office credential revoke`.
- For the desktop plugin, run `office connect` again with the replacement file and open a fresh session.
- For a checkout incident, suspend new work, save the affected runner's state and follow [coordinator recovery](checkouts.md#recover-the-coordinator).

[Back up and restore](backup.md) · [Release packaging](release.md)
