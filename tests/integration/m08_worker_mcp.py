"""Run two isolated worker processes that use their own controller-local MCP socket."""

import argparse
import json
import os
import re
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from workspace_fixture import ROOT, workspace_fixture


MCP_WORKER = r"""
set -eu
printf '%s\n' socket > /work/stage
test -S "$COCODEOP_MCP_SOCKET"
test -z "${OFFICE_TOKEN-}"
test "$(awk 'END { print NR }' /proc/net/route)" = 1
printf '%s\n' frames > /work/stage
{
printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"oci-worker-fixture","version":"1.0"}}}'
printf '%s\n' '{"jsonrpc":"2.0","method":"notifications/initialized"}'
printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}'
printf '%s\n' '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"office_run_context","arguments":{}}}'
} | /usr/local/bin/office-runner worker-mcp > /work/responses.jsonl 2>/work/transport-error
test "$(wc -l < /work/responses.jsonl)" = 3
grep -q '"name":"cocodeop-run"' /work/responses.jsonl
grep -q '"name":"office_run_context"' /work/responses.jsonl
grep -q "$1" /work/responses.jsonl
if grep -q "$2" /work/responses.jsonl; then exit 8; fi
if grep -q 'crun_' /work/responses.jsonl; then exit 9; fi
printf '%s\n' barrier > /work/stage
touch /work/ready
until test -e /work/release; do sleep 0.1; done
printf '{"kind":"succeeded","summary":"Verified the live worker MCP context"}' > "$COCODEOP_RESULT"
"""


def verify(office, tmp, guest, binary, image_id):
    runners = {}
    for label, peer, owner in [("alpha", "beta", office.owner_a), ("beta", "alpha", office.owner_b)]:
        lead, _ = office.agent(owner, f"Controller lead {label}", "lead")
        agent, identity = office.agent(owner, f"MCP worker {label}", "worker")
        assert configure(office, owner, 20).get("budget")
        runner_id, bearer = register_runner(office, owner, f"Local MCP controller {label}")
        runners[label] = {"owner": owner, "lead": lead, "agent": agent, "identity": identity, "id": runner_id,
                          "bearer": bearer, "task": f"MCP authorized for {label}", "peer_task": f"MCP authorized for {peer}"}

    host = RunnerHost(tmp, guest, binary)
    host.setup({label: item["bearer"] for label, item in runners.items()})

    def configure_runner(label, item):
        manifest = {"adapter_id": "oci-mcp-fixture.v1", "image_id": image_id, "entrypoint": "/bin/sh",
                    "args": ["-c", MCP_WORKER, "worker", item["task"], item["peer_task"]],
                    "timeout_seconds": 50, "memory_mib": 128, "cpus": 1, "pids_limit": 64, "run_mcp": True}
        file = f"manifest-{label}.json"
        host.replace(file, json.dumps(manifest))
        host.invoke(["chmod", "600", f"{host.root}/{file}"], check=True)
        receipt = host.invoke([host.installed, "runtime-manifest", "--file", f"{host.root}/{file}", "--json"])
        assert receipt.returncode == 0, receipt.stderr
        managed = {"adapter_id": manifest["adapter_id"], "reservation_units": 10,
                   "manifest_sha256": json.loads(receipt.stdout)["manifest_sha256"]}
        runtime = office.tool(item["owner"], "office_create_runtime_template", name=f"MCP runtime {label}", kind="managed",
                              managed=managed, idempotency_key=str(uuid4()))["template_id"]
        item["state"] = f"{host.root}/state-{label}"
        item["config"] = f"controller-{label}.json"
        host.invoke(["mkdir", "-m", "700", item["state"]], check=True)
        host.replace(item["config"], json.dumps({"capabilities": ["linux"], "runtimes": [
            {"template_id": runtime, "manifest": manifest},
        ]}))
        host.invoke(["chmod", "600", f"{host.root}/{item['config']}"], check=True)
        task = office.tool(item["lead"], "office_create_task", objective=item["task"],
                           acceptance_criteria=["Read only your own task through MCP"], idempotency_key=str(uuid4()))["task"]
        prepared = office.tool(item["lead"], "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                               idempotency_key=str(uuid4()))["task"]
        item["job"] = enqueue(office, item["lead"], prepared, item["identity"], runtime)["job"]

    def launch(label, item):
        forward, endpoint = host.connection_for_runner(office.endpoint)
        return host.invoke([host.installed, "serve", "--once", "--json", "--url", f"http://{endpoint}",
                            "--state-dir", item["state"], "--file", f"{host.root}/{item['config']}",
                            "--token-file", f"{host.root}/{label}.token"], forward=forward)

    def observe(label, item):
        view = office.tool(item["agent"], "office_get_task", task_id=item["job"]["task_id"])
        if not view["runs"] or view["runs"][0]["status"] != "running":
            return False
        run = view["runs"][0]["run_id"]
        scratch = f"{item['state']}/run-{run}/scratch"
        if host.invoke(["test", "-f", f"{scratch}/ready"]).returncode != 0:
            return False
        item["scratch"] = scratch
        journal = host.invoke(["cat", f"{item['state']}/current.json"], check=True).stdout
        assert "crun_" not in journal and item["bearer"] not in journal
        output = host.invoke(["cat", f"{scratch}/responses.jsonl"], check=True).stdout
        frames = [json.loads(line) for line in output.splitlines()]
        document = frames[2]["result"]["structuredContent"]
        assert document["run_id"] == run and document["task"]["objective"] == item["task"], label
        assert document["executor"]["agent_id"] == item["identity"]["agent_id"]
        return True

    try:
        for label, item in runners.items():
            configure_runner(label, item)
        with ThreadPoolExecutor(max_workers=2) as pool:
            futures = {label: pool.submit(launch, label, item) for label, item in runners.items()}
            deadline = time.monotonic() + 30
            waiting = dict(runners)
            while waiting and time.monotonic() < deadline:
                for label, item in list(waiting.items()):
                    if futures[label].done():
                        failure = futures[label].result()
                        view = office.tool(item["agent"], "office_get_task", task_id=item["job"]["task_id"])
                        details = {}
                        if view["runs"]:
                            scratch = f"{item['state']}/run-{view['runs'][0]['run_id']}/scratch"
                            for evidence in ("stage", "responses.jsonl", "transport-error"):
                                raw = host.invoke(["head", "-c", "4096", f"{scratch}/{evidence}"]).stdout
                                details[evidence] = re.sub(r"crun_[a-fA-F0-9]{64}", "[redacted]", raw.replace(item["bearer"], "[redacted]"))
                        raise AssertionError((label, failure.stderr, failure.stdout, details))
                    if observe(label, item):
                        del waiting[label]
                if waiting:
                    time.sleep(0.3)
            assert not waiting, f"Workers never reached the test barrier: {list(waiting)}"
            assert len({item["scratch"] for item in runners.values()}) == 2
            assert office.sql("SELECT count(DISTINCT digest) FROM run_credentials") == "2"
            for item in runners.values():
                host.invoke(["touch", f"{item['scratch']}/release"], check=True)
            for label, item in runners.items():
                result = futures[label].result(timeout=25)
                assert result.returncode == 0, result.stderr
                completed = json.loads(result.stdout)["receipt"]["execution"]["run"]
                assert completed["status"] == "succeeded" and completed["lease"]["runner_id"] == item["id"]
                assert completed["lease"]["job_id"] == item["job"]["job_id"]
        return ["two_local_containers_hold_distinct_run_credentials_and_round_trip_real_mcp_over_private_unix_sockets",
                "socket_worker_receives_only_its_own_task_and_result_while_tokens_remain_outside_the_container"]
    finally:
        host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    parser.add_argument("--image-id", required=True)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-worker-socket-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "worker-socket") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary, args.image_id),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "podman_image_id": args.image_id, "independent_physical_hosts": False}))
