# Get started

Run the Core on a Linux server. Each participating owner connects a lead and runs a Linux runner on their own computer. Repeat the owner setup whenever someone joins. Use the same release version across the installation.

| Where | What runs there |
| --- | --- |
| Server | CoCodeOp Core and PostgreSQL |
| Each owner's computer | Their Codex plugin and Linux runner |
| Checkout host | The project's shared source directory and its coordinator |

Use Linux amd64 for Core and for each runner. On Windows, run the owner-side Linux commands in a local Linux VM. The Codex desktop app and `office.exe` can stay on Windows. Each owner needs a provider key for the OpenAI worker setup below. Signing in to the Codex app covers the lead's session.

## 1. Start the Core

On the server, install Docker with Compose and Python 3.11 or newer. Unpack the release into `$HOME/cocodeop-release` and run these commands as the installation operator.

```bash
cd "$HOME/cocodeop-release"
sha256sum -c --quiet SHA256SUMS && docker image load --input server-linux-amd64.tar

cd support
install -d -m 700 "$HOME/.config/cocodeop-core"
export COCODEOP_SECRETS_DIR="$HOME/.config/cocodeop-core/db"
if [ ! -d "$COCODEOP_SECRETS_DIR" ]; then
  python3 deploy/init-secrets.py "$COCODEOP_SECRETS_DIR"
fi

core() {
  docker compose --env-file server.env -p cocodeop \
    -f deploy/compose.yaml -f deploy/compose.release.yaml "$@"
}
core up --no-build --wait
core exec -T server office status --json
```

Keep the Compose project name, database volume, and secrets directory for subsequent starts. The server publishes port `8400` on loopback. The readiness check should report schema `67`.

### Give owners a stable URL

With an HTTPS domain, point its DNS at the server and reverse proxy to `127.0.0.1:8400`. For Caddy installed on the server host, add a site like this, using your actual domain, then validate and reload Caddy. Clients use the origin `https://office.example.org`. [Caddy reverse proxy guide](https://caddyserver.com/docs/quick-starts/reverse-proxy)

```caddyfile
office.example.org {
    reverse_proxy 127.0.0.1:8400
}
```

For a private test with SSH access, run this tunnel on each machine that needs the Core. On a Windows computer with a Linux VM, open a tunnel in Windows for the desktop plugin and another inside the VM for its runner. Keep each tunnel open and use `http://127.0.0.1:8400` in that environment.

```bash
ssh -N -o ExitOnForwardFailure=yes -L 127.0.0.1:8400:127.0.0.1:8400 user@server
```

## 2. Create a workspace and invite owners

In the Core terminal from step 1, create the first owner and workspace. Run this once per installation with an empty operator directory.

```bash
umask 077
operator_dir="$HOME/.config/cocodeop-core/operator"
install -d -m 700 "$operator_dir"

core run --rm --no-deps --user "$(id -u):$(id -g)" \
  --entrypoint /usr/local/bin/office \
  --volume "$operator_dir:/operator" \
  server bootstrap --user "Workspace Owner" --workspace Engineering \
  --token-file /operator/owner.token --json > "$operator_dir/owner.json"
```

Read `memberships[0].workspace_id` in `owner.json` and keep that ID for onboarding. Transfer `owner.token` privately to the first owner's machine. Each owner uses a private directory for their own files.

Prepare the CLI on each owner's Linux machine. Install rootless Podman and `jq` first. On Debian, the Podman installation should include `uidmap` and a working user session. Install Git on the machine that will host the shared checkout.

```bash
bundle="$HOME/cocodeop-release"
cfg="$HOME/.config/cocodeop"
umask 077
install -d -m 700 "$HOME/.local/bin" "$cfg"
tar -xzf "$bundle/linux-amd64.tar.gz" -C "$HOME/.local/bin" office office-runner
export PATH="$HOME/.local/bin:$PATH"
export OFFICE_URL='https://office.example.org'
export WORKSPACE_ID='PASTE_WORKSPACE_ID'
office status --url "$OFFICE_URL" --json
```

If using the SSH tunnel, set `OFFICE_URL` to its loopback origin. Put the first owner's `owner.token` in their private `$cfg` directory with mode `0600`. With that credential, create a fresh invitation file for each additional owner.

```bash
invite_name='Contributor 01'
invite_file="$cfg/invite-contributor-01.token"
OFFICE_TOKEN="$(cat "$cfg/owner.token")" office user invite \
  --url "$OFFICE_URL" --workspace "$WORKSPACE_ID" --name "$invite_name" \
  --invitation-file "$invite_file" --json
```

Send each invitee the workspace ID, URL, and their invitation file privately. Each invitee saves the file as `$cfg/invite.token` and creates their own human credential. Workspace membership is separate from the lead role used by their agent.

```bash
chmod 600 "$cfg/invite.token"
office user join --url "$OFFICE_URL" --invitation-file "$cfg/invite.token" \
  --token-file "$cfg/owner.token" --json
```

## 3. Register one lead and runner per owner

Run this on each owner's Linux machine. Choose a short label for the lead and runner.

```bash
owner_label=contributor-01
export OFFICE_TOKEN="$(cat "$cfg/owner.token")"

office agent create --url "$OFFICE_URL" --workspace "$WORKSPACE_ID" \
  --name "lead-$owner_label" --role lead --token-file "$cfg/lead.token" --json \
  > "$cfg/lead.json"
office runner register --url "$OFFICE_URL" --workspace "$WORKSPACE_ID" \
  --name "runner-$owner_label" --token-file "$cfg/runner.token" --json \
  > "$cfg/runner.json"

jq -r '.agent_id' "$cfg/lead.json"
jq -r '.runner_id' "$cfg/runner.json"
unset OFFICE_TOKEN
```

Use your own human token for setup. The plugin uses your `lead.token`. The runner uses `runner.token` and reads its provider key from a separate local file.

Follow [Start a local Codex runner](runners.md#start-a-local-codex-runner) on each owner's Linux machine. Keep each runner running and note its runner ID and runtime template ID.

## 4. Connect the Codex plugin

On each computer running Codex, put the release's `office` CLI on the desktop app's `PATH`. On Windows, rename `office-windows-amd64.exe` to `office.exe`, add its directory to your user `Path`, and reopen Codex after the change. Copy only your own `lead.token` to a private path that the Codex application can read if the lead and runner use different operating systems.

From the release's `support` directory, run the following in a terminal using the same Codex user profile as the app. Use the server origin without `/mcp` and the agent ID from step 3.

```text
office connect --url https://office.example.org --agent LEAD_AGENT_ID --token-file ABSOLUTE_PRIVATE_LEAD_TOKEN_PATH --json
codex plugin marketplace add .
codex plugin add cocodeop@cocodeop-local
codex plugin list --marketplace cocodeop-local
```

For the desktop app, restart it and select **CoCodeOp** from the local **CoCodeOp** marketplace in Plugins if it still needs installation. Open a new local Codex chat. Each lead should report its own agent and owner IDs and the common workspace ID. [Codex plugin installation](https://developers.openai.com/plugins/build/plugins)

```text
Use CoCodeOp. Call office_bootstrap and read the workspace context.
Tell me the workspace ID, my owner ID and my lead agent ID.
```

## 5. Prove the workers run locally

In each lead chat, give this prompt with that owner's runtime template ID. Use a worker name suited to the team.

> Use CoCodeOp to create your own team and a worker named Backend Bob. Give Bob persistent instructions to report the task ID and a short result. Put him on your team. Create a coordination task asking him to read his CoCodeOp run context and return the line “runner check complete” with the task ID. Move the task to ready and queue managed execution with runtime template **YOUR_TEMPLATE_ID**. Report the worker name, task ID and job ID. When the result arrives, review it and return the run ID and runner ID.

On each owner's Linux machine, watch for the worker container and then match its local evidence to the same run shown in the lead chat.

```bash
podman ps --filter name=cocodeop- --format '{{.ID}} {{.Names}} {{.Image}}'

run_id='PASTE_RUN_ID'
state="$HOME/.local/state/cocodeop/runner"
jq -r '.backend, .session_id, .exit_code, .events_sha256' \
  "$state/run-$run_id/scratch/agent/evidence.json"
sha256sum "$state/run-$run_id/scratch/agent/events.jsonl"
```

Expect the owner-specific runner ID, a Codex session ID, exit code `0`, and a matching event log hash. Each machine should have its own run directory for its task. If a container finishes before you can see it, use the run directory and the active runner ID. For a stronger check, stop one owner's only runner with Ctrl+C while it is idle and then queue another task for that owner. Their task should wait while other owners can continue work. Restart the stopped runner and confirm that its queued task runs there.

Ask the lead **“How is Backend Bob doing?”** It can query `office_worker_status` by name and report Bob's current task, recent attempts and review state.

## 6. Work on a shared project

[Register one shared checkout](checkouts.md) on the Linux machine that holds the source. Grant each lead the paths and commands their team needs. Then give each lead a goal and ask it to propose a division of work, pick team members and dispatch the tasks.

- **Start work.** “Make a plan with the leads affected by the API. Assign Backend Bob the implementation and record the agreed contract before editing it.”
- **Track progress.** “Check Bob's worker status and the current task. Summarize his latest result, blockers and next action.”
- **Review together.** “Create a snapshot, ask the other team to run the approved contract checks, and review the results for that exact snapshot.”

[Runner setup](runners.md) · [Diagnose an installation](operations.md) · [Back up and restore](backup.md)
