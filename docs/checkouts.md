# Shared checkouts

Register one real source directory on a Linux host. The Core routes scoped operations to its coordinator. Workers on different owners' runners can use the same checkout ID.

## Register a project

On the checkout host, run as the Linux user who owns the source. Use a local Linux filesystem and a real directory. Put the coordinator's private state on the same filesystem as the checkout and outside the source tree. The coordinator user should own the writable source files and directories.

Continue with the first owner's `cfg`, `OFFICE_URL` and `WORKSPACE_ID` from the [installation guide](getting-started.md). This example publishes metadata to the workspace and exposes only files listed in the manifest.

```bash
root="$HOME/projects/example"
export OFFICE_TOKEN="$(cat "$cfg/owner.token")"
uuid() { cat /proc/sys/kernel/random/uuid; }

office runner register --url "$OFFICE_URL" --workspace "$WORKSPACE_ID" \
  --name checkout-host --token-file "$cfg/checkout-host.token" --json \
  > "$cfg/checkout-host.json"
host_id="$(jq -er '.runner_id' "$cfg/checkout-host.json")"

jq -n --arg ws "$WORKSPACE_ID" --arg key "$(uuid)" \
  '{workspace_id:$ws, name:"Example", visibility:{kind:"workspace"}, idempotency_key:$key}' \
  > "$cfg/project.json"
project_id="$(office checkout create-project --url "$OFFICE_URL" \
  --file "$cfg/project.json" --json | jq -er '.project.project_id')"

cat > "$cfg/source-manifest.json" <<'JSON'
{"include":[{"kind":"directory","path":"src"},{"kind":"file","path":"README.md"}]}
JSON
office-runner checkout inspect --root "$root" --manifest "$cfg/source-manifest.json" \
  --json > "$cfg/checkout-preview.json"
```

Adapt the manifest to match your project. Inspect the listed files and hashes before enrolling it. Use `exclude` entries for private or generated content inside an included directory. The gateway also filters reserved paths such as `.git` and local secret files.

```bash
jq -n --arg ws "$WORKSPACE_ID" --arg project "$project_id" --arg host "$host_id" \
  --arg key "$(uuid)" --slurpfile manifest "$cfg/source-manifest.json" \
  '{workspace_id:$ws, project_id:$project, host_runner_id:$host,
    idempotency_key:$key, manifest:$manifest[0]}' > "$cfg/checkout-request.json"
checkout_id="$(office checkout request --url "$OFFICE_URL" \
  --file "$cfg/checkout-request.json" --json | jq -er '.checkout.checkout_id')"

source_hash="$(jq -er '.baseline.manifest_sha256' "$cfg/checkout-preview.json")"
office-runner checkout enroll --url "$OFFICE_URL" --checkout "$checkout_id" \
  --root "$root" --token-file "$cfg/checkout-host.token" \
  --expected-source-hash "$source_hash" --json > "$cfg/checkout.json"
printf 'Checkout ID: %s\n' "$checkout_id"
```

Start the coordinator in a separate terminal under the same Linux user. Supply the paths, URL and checkout ID from above.

```bash
checkout_state="$HOME/projects/.cocodeop-checkout-state"
install -d -m 700 "$checkout_state"
office-runner checkout serve --url "$OFFICE_URL" --checkout "$checkout_id" \
  --root "$root" --state-dir "$checkout_state" --token-file "$cfg/checkout-host.token"
```

Keep the coordinator and participating owners' worker controllers running. A coordinator outage pauses file operations for that checkout. Other workspace coordination can continue.

## Grant access to leads

The checkout owner supplies a separate grant for each lead. Take the lead's `user_id` and `agent_id` from its verified identity. Start with separate write areas, a short expiry, and the applicable read dependencies. The example grants read access to `src` and permits writing within its existing `backend` directory. Adjust this for each team.

```bash
lead_owner_id='PASTE_LEAD_OWNER_ID'
lead_agent_id='PASTE_LEAD_AGENT_ID'
checkout_revision="$(office checkout get --url "$OFFICE_URL" \
  --workspace "$WORKSPACE_ID" --id "$checkout_id" --json | jq -er '.revision')"
jq -n --arg ws "$WORKSPACE_ID" --arg checkout "$checkout_id" \
  --arg user "$lead_owner_id" --arg agent "$lead_agent_id" \
  --arg expires "$(date -u -d '+1 day' '+%Y-%m-%dT%H:%M:%SZ')" \
  --arg key "$(uuid)" --argjson rev "$checkout_revision" \
  '{workspace_id:$ws, checkout_id:$checkout, expected_revision:$rev, idempotency_key:$key,
    recipient:{user_id:$user,agent_id:$agent}, expires_at:$expires,
    permissions:{read:{allow:[{kind:"directory",path:"src"}]},
      write:{allow:[{kind:"directory",path:"src/backend"}]},
      write_operations:["create","replace"]}}' > "$cfg/checkout-grant.json"
office scope grant --url "$OFFICE_URL" --file "$cfg/checkout-grant.json" --json
```

Give each lead its grant ID and the shared checkout ID. Ask it to prepare a coding task and set a narrower WorkScope inside the grant. The runner uses the scope revision once the coordinator acknowledges it. To add a shared write area, name the responsible leads as approvers when defining that area. A worker can request an access change through MCP if a task needs it.

## Publish and verify

- Workers read the latest visible files through `office_read_file`. They publish a change with the expected file hashes and any required read dependencies. The gateway checks the scope, claims and dependencies before writing.
- For an overlapping path, ask the current claimant for a handoff. Read the current file again after the handoff before publishing. Changes with stale hashes are rejected.
- For tests, register a fixed command template with `office scope create-command`. On the checkout host, use `office-runner checkout command-manifest` to bind the returned spec to a pinned, local Podman image in the coordinator's `--commands-file`. The file's `commands` array uses `template_id`, `spec_sha256`, `image_id` and `entrypoint`. Add the template ID to the lead grant and task scope. Keep package dependencies in the command image or approved caches.
- Capture a source snapshot before final review. Ask a separate worker to run the approved contract test with that `source_snapshot_id` and save its command ID. Review the task results and checks against that exact snapshot. The checkout owner or their lead can set a release policy and request an accepted checkpoint.

Snapshot and review commands are available under `office checkout --help`. The [checkout snapshot schema](../schemas/checkout-snapshot.v1.json), [command template schema](../schemas/command-template.v1.json) and [release schema](../schemas/checkout-release.v1.json) describe their receipts.

## Recover the coordinator

Restart with the same root, local state directory and runner credential. If recovery reports a mismatch, preserve the unexpected content and compare it with the local publication receipts. Once the owner has resolved the mismatch, start the coordinator with `--reconcile`. [Backup and restore](backup.md)
