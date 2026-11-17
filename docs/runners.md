# Local runners

Each owner authorizes the image, provider and resource limits for their own runner. A lead can create and schedule workers against the owner's approved templates. The runner uses rootless Podman on the owner's Linux machine.

## Start a local Codex runner

Complete [owner registration](getting-started.md#3-register-one-lead-and-runner-per-owner) first. Continue in the same Bash session on the owner's Linux machine with `bundle`, `cfg`, `OFFICE_URL` and `WORKSPACE_ID` set.

### Load the release image

Run as your ordinary Linux user. Podman should report rootless mode and cgroup v2. Use its local image store.

```bash
podman info --format '{{.Host.Security.Rootless}} {{.Host.CgroupsVersion}}'
podman load --input "$bundle/worker-codex-linux-amd64.tar"
image="$(jq -er '.images.codex.oci_config_digest' "$bundle/release.json")"
podman image inspect --format '{{.Id}}' "$image"
```

Compare the image ID with `oci_config_digest` in `release.json`. Podman can display the same digest without the `sha256:` prefix.

### Choose the worker's API and model

Codex workers use a Responses-compatible endpoint. This example uses the OpenAI API. Enter a model ID your provider key can use. Set the endpoint to your provider's HTTPS API prefix, including `/v1` where needed.

```bash
umask 077
read -r -p 'Model ID: ' model
read -r -s -p 'Worker provider key: ' provider_key
printf '\n'
printf '%s\n' "$provider_key" > "$cfg/provider.token"
unset provider_key

jq -n --arg model "$model" \
  '{name:"codex.responses", base_url:"https://api.openai.com/v1", model:$model,
    api:"responses", max_requests:32, max_output_tokens:4096, timeout_seconds:160}' \
  > "$cfg/provider.json"
provider_sha="$(office-runner inference-profile --file "$cfg/provider.json" --json |
  jq -er '.provider_profile_sha256')"
```

The host adds the credential when it sends a request. Workers receive a local socket and the model name. Only loopback providers can use an HTTP URL. For a trusted local provider without authentication, omit the `bearer_file` field from `controller.json`. Some compatible Responses providers require `flatten_response_tools: true`.

### Approve the runtime and budget

The manifest fixes the image, executable, arguments and container limits. Enabling `checkout_gateway` also makes this template eligible for coding tasks with an approved WorkScope.

```bash
jq -n --arg image "${image#sha256:}" --arg profile "$provider_sha" \
  '{adapter_id:"codex-exec.v1", image_id:$image, entrypoint:"/usr/local/bin/office-runner",
    args:["worker-agent","--backend","codex"], timeout_seconds:390, memory_mib:1536,
    cpus:1, pids_limit:128, run_mcp:true, checkout_gateway:true,
    provider_profile_sha256:$profile}' > "$cfg/manifest.json"

manifest_sha="$(office-runner runtime-manifest --file "$cfg/manifest.json" --json |
  jq -er '.manifest_sha256')"
uuid() { cat /proc/sys/kernel/random/uuid; }

jq -n --arg ws "$WORKSPACE_ID" --arg digest "$manifest_sha" --arg key "$(uuid)" \
  '{workspace_id:$ws, name:"Local Codex", kind:"managed",
    managed:{adapter_id:"codex-exec.v1", reservation_units:15, manifest_sha256:$digest},
    idempotency_key:$key}' > "$cfg/runtime.json"
jq -n --arg ws "$WORKSPACE_ID" --arg key "$(uuid)" \
  '{workspace_id:$ws, expected_revision:0, limit_units:150, idempotency_key:$key}' \
  > "$cfg/budget.json"

export OFFICE_TOKEN="$(cat "$cfg/owner.token")"
runtime_id="$(office delegation create-runtime --url "$OFFICE_URL" \
  --file "$cfg/runtime.json" --json | jq -er '.template_id')"
office scheduler set-budget --url "$OFFICE_URL" --file "$cfg/budget.json" --json
unset OFFICE_TOKEN

jq -n --arg id "$runtime_id" --arg key "$cfg/provider.token" \
  --slurpfile manifest "$cfg/manifest.json" --slurpfile profile "$cfg/provider.json" \
  '{capabilities:["linux"], runtimes:[{template_id:$id, manifest:$manifest[0]}],
    providers:[{profile:$profile[0], bearer_file:$key}]}' > "$cfg/controller.json"
printf 'Runtime template ID: %s\n' "$runtime_id"
```

The budget above is for a fresh owner. To edit an existing one, read its revision with `office scheduler budget` and use that revision instead of `0`. Execution units reserve queue capacity. Provider billing follows the provider's own limits.

### Start the controller

Keep its state outside the checkout on the owner's Linux filesystem. Use the same state directory when restarting the same registered runner.

```bash
state="$HOME/.local/state/cocodeop/runner"
install -d -m 700 "$state"

office-runner doctor --url "$OFFICE_URL" --file "$cfg/controller.json" \
  --token-file "$cfg/runner.token" --state-dir "$state" --json
office-runner serve --url "$OFFICE_URL" --file "$cfg/controller.json" \
  --token-file "$cfg/runner.token" --state-dir "$state"
```

One controller advertises one slot. It can run any of its configured templates. Keep the runner in a terminal for the first check. For background operation, use a service under the same Linux user with the exact argument paths above.

## Other models and clients

| Worker | Adapter | Arguments | API |
| --- | --- | --- | --- |
| Codex | `codex-exec.v1` | `["worker-agent","--backend","codex"]` | `responses` |
| OpenCode | `opencode-run.v1` | `["worker-agent","--backend","opencode"]` | `chat_completions` |

Use the matching worker image and update the manifest's adapter ID and arguments as well as the managed adapter field in the runtime request. Inference profiles bind the endpoint, model and limits by hash. Register the new manifest as a new runtime when these fields change. To rotate a provider key, replace the private file at the same path.

A custom `generic-jsonl.v1` adapter uses an owner-approved OCI image and manifest. It receives the task in `/task.json` and writes the result to `/work/result.json`. The entrypoint should complete with code 0 and a result such as `{"kind":"succeeded","summary":"…"}`. [Supported profiles](compatibility.md)

## Diagnose or replace a runner

Each attempt leaves a local `run-<run_id>` directory with the worker's normalized log and evidence. Use `office doctor` for MCP connectivity, `office task explain` for queue eligibility and `office runs logs` for the server's record. Keep local logs private because they can include task content.

For maintenance, pause admission and let the runner reconcile active attempts before replacing its credential or machine. A replacement on a new machine needs its own registration. Reuse the original state directory when restarting on the same machine. [Operations](operations.md)
