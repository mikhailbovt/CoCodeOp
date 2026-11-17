"""Run an approved external executable under the versioned backend protocol."""

import argparse
import json
import os
import tempfile
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from m09_native_agents import install_json
from workspace_fixture import ROOT, workspace_fixture


def install_bridge(office, host, owner, image, checkout=False):
    manifest = {
        "adapter_id": "generic-jsonl.v1", "image_id": image, "entrypoint": "/usr/local/bin/office-runner",
        "args": ["worker-agent", "--backend", "jsonl", "--bridge", "/opt/cocodeop/bridge"],
        "run_mcp": True, "checkout_gateway": checkout, "timeout_seconds": 60, "memory_mib": 128, "cpus": 1, "pids_limit": 64,
    }
    manifest_file = install_json(host, "bridge-manifest.json", manifest)
    digest = host.invoke([host.installed, "runtime-manifest", "--file", manifest_file, "--json"], check=True)
    template = office.tool(owner, "office_create_runtime_template", name="JSONL bridge fixture", kind="managed",
                           managed={"adapter_id": manifest["adapter_id"], "reservation_units": 10,
                                    "manifest_sha256": json.loads(digest.stdout)["manifest_sha256"]},
                           idempotency_key=str(uuid4()))["template_id"]
    config = install_json(host, "bridge-controller.json", {
        "capabilities": ["linux"], "runtimes": [{"template_id": template, "manifest": manifest}],
    })
    state = f"{host.root}/bridge-state"
    host.invoke(["mkdir", "-m", "700", state], check=True)
    return template, config, state


def verify(office, host, image):
    owner = office.owner_b
    lead, _ = office.agent(owner, "Bridge lead", "lead")
    _, agent = office.agent(owner, "Bridge worker", "worker")
    _, token = register_runner(office, owner, "Approved generic bridge runner")
    host.setup({"runner": token})
    assert configure(office, owner, 40).get("budget")
    template, config, state = install_bridge(office, host, owner, image)
    runs = []
    for objective, expected in (("Verify the assigned MCP run identity", "succeeded"),
                                ("Reject the bridge's unknown terminal event", "failed")):
        task = office.tool(lead, "office_create_task", objective=objective, acceptance_criteria=["Check the bridge protocol"],
                           idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                            idempotency_key=str(uuid4()))["task"]
        enqueue(office, lead, ready, agent, template)
        forward, endpoint = host.connection_for_runner(office.endpoint)
        report = host.invoke([host.installed, "serve", "--file", config, "--state-dir", state,
                              "--url", f"http://{endpoint}", "--token-file", f"{host.root}/runner.token",
                              "--once", "--json"], forward=forward, timeout=60, check=True)
        observed = json.loads(report.stdout)
        receipt = observed["receipt"]
        run = receipt.get("execution", receipt)["run"]
        if run["status"] != expected or run["executor"]["agent_id"] != agent["agent_id"]:
            scratch = f"{state}/run-{run['run_id']}/scratch/agent"
            diagnostics = host.invoke(["tail", "-c", "2200", f"{scratch}/native.stderr"]).stdout
            raise AssertionError((observed, report.stderr, diagnostics))
        evidence = run["backend"]
        assert evidence["version"] == 1 and evidence["session_id"].startswith("test-")
        if expected == "succeeded":
            assert evidence["checkpoint_id"].startswith("sha256:")
            assert evidence["usage"]["input_tokens"] == 50
            result = office.tool(lead, "office_get_task_result", task_id=task["task_id"])
            artifact = result["content"]["artifacts"][0]
            assert artifact["name"] == "run-identity" and json.loads(artifact["content"])["run_id"] == run["run_id"]
            phases = ["start", "checkpoint-1", "finish"]
            for phase in phases:
                snapshot = f"{state}/run-{run['run_id']}/scratch/agent/inbox-{phase}.json"
                work = json.loads(host.invoke(["cat", snapshot], check=True).stdout)
                assert work["run"]["run_id"] == run["run_id"]
                assert all(e["payload"]["task_id"] == task["task_id"] for e in work["inbox"]["events"])
        else:
            assert evidence["terminal_state"] == "failed"
            assert "unknown or malformed" in run["summary"]
        runs.append({"run_id": run["run_id"], "state": run["status"], "backend": evidence})
    return {"image": image, "test_backend": "protocol fixture executable", "runs": runs,
            "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--image-id", required=True)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-generic-bridge-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "generic-bridge") as office:
            host = RunnerHost(root, args.linux_vm, binary)
            try:
                print(json.dumps(verify(office, host, args.image_id)))
            finally:
                host.close()
