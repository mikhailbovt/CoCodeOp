"""Drive both packaged agent CLIs through the real owner-side runner and FreeLLMAPI."""

import argparse
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from workspace_fixture import ROOT, workspace_fixture

sys.path.insert(0, str(ROOT / "tests/protocol"))
from freellmapi_live import GATEWAY_MODEL, IMAGE, gateway  # noqa: E402


BACKENDS = {
    "codex": ("codex-exec.v1", "responses", "0.148.0"),
    "opencode": ("opencode-run.v1", "chat_completions", "1.19.5"),
}


def install_json(host, name, value):
    host.replace(name, json.dumps(value))
    path = f"{host.root}/{name}"
    host.invoke(["chmod", "600", path], check=True)
    return path


def install_backend(office, host, owner, backend, image, endpoint, *, checkout=False, model=GATEWAY_MODEL,
                    authenticated=True):
    adapter, wire, _ = BACKENDS[backend]
    profile = {
        "name": f"free-local.{backend}", "base_url": f"http://{endpoint}/v1",
        "model": model, "api": wire, "max_requests": 32,
        "max_output_tokens": 4096, "timeout_seconds": 160,
    }
    if backend == "codex":
        profile["flatten_response_tools"] = True
    profile_file = install_json(host, f"{backend}-inference.json", profile)
    described = host.invoke([host.installed, "inference-profile", "--file", profile_file, "--json"], check=True)
    manifest = {
        "adapter_id": adapter, "image_id": image, "entrypoint": "/usr/local/bin/office-runner",
        "args": ["worker-agent", "--backend", backend], "timeout_seconds": 390,
        "memory_mib": 1536, "cpus": 1, "pids_limit": 128, "run_mcp": True,
        "checkout_gateway": checkout, "provider_profile_sha256": json.loads(described.stdout)["provider_profile_sha256"],
    }
    manifest_file = install_json(host, f"{backend}-manifest.json", manifest)
    reviewed = host.invoke([host.installed, "runtime-manifest", "--file", manifest_file, "--json"], check=True)
    runtime = office.tool(owner, "office_create_runtime_template", name=f"{backend} via FreeLLMAPI", kind="managed",
                          managed={"adapter_id": adapter, "reservation_units": 15,
                                   "manifest_sha256": json.loads(reviewed.stdout)["manifest_sha256"]},
                          idempotency_key=str(uuid4()))["template_id"]
    provider = {"profile": profile}
    if authenticated:
        provider["bearer_file"] = f"{host.root}/provider.token"
    controller = install_json(host, f"{backend}-controller.json", {
        "capabilities": ["linux"], "runtimes": [{"template_id": runtime, "manifest": manifest}],
        "providers": [provider],
    })
    state = f"{host.root}/{backend}-state"
    host.invoke(["mkdir", "-m", "700", state], check=True)
    return runtime, controller, state


def invoke(backend, host, service, forwards, config, state):
    result = host.invoke([host.installed, "serve", "--file", config, "--state-dir", state,
                          "--token-file", f"{host.root}/{backend}.token", "--url", f"http://{service}",
                          "--once", "--json"], forward=forwards, timeout=420)
    assert result.returncode == 0, (result.stdout, result.stderr)
    report = json.loads(result.stdout)
    if result.stderr:
        report["diagnostics"] = result.stderr
    return report


def get_attempt(host, state, execution, *, require_success=True):
    receipt = execution.get("receipt", {})
    run = receipt.get("execution", receipt).get("run")
    assert run, execution
    scratch = f"{state}/run-{run['run_id']}/scratch/agent"
    collected = host.invoke(["cat", f"{scratch}/events.jsonl"])
    assert collected.returncode == 0, (execution, collected.stderr)
    log = collected.stdout
    evidence = json.loads(host.invoke(["cat", f"{scratch}/evidence.json"], check=True).stdout)
    assert evidence["events_sha256"] == hashlib.sha256(log.encode()).hexdigest()
    events = list(map(json.loads, log.splitlines()))
    assert len(events) == evidence["event_count"] and all(event["version"] == 1 for event in events)
    assert run["backend"] == evidence
    if run["status"] != "succeeded":
        if not require_success:
            return run, evidence, events
        diagnostic = host.invoke(["tail", "-c", "2500", "--", f"{scratch}/native.stderr"]).stdout
        reason = host.invoke(["tail", "-c", "1300", "--", f"{scratch}/native-error.json"]).stdout
        raise AssertionError((execution, evidence, events[-12:], diagnostic, reason))
    assert evidence["terminal_state"] == "completed" and evidence["exit_code"] == 0
    assert evidence["usage"]["input_tokens"] > 0 and evidence["usage"]["output_tokens"] > 0
    assert events[0]["kind"] == "started" and events[0]["session_id"] == evidence["session_id"]
    assert events[-1]["kind"] == "completed"
    tools = {event["tool"] for event in events if event.get("successful") and "tool" in event}
    assert "office_run_context" in tools
    return run, evidence, events


def verify(office, host, images, url, key):
    tokens, prepared = {"provider": key}, {}
    coordinator_forward, service = host.connection_for_runner(office.endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    while endpoint == service:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(url).netloc)
    forwards = [route for route in (coordinator_forward, provider_forward) if route]
    for backend, image in images.items():
        owner = office.owner_a if backend == "codex" else office.owner_b
        lead, _ = office.agent(owner, f"{backend} runner lead", "lead")
        _, agent = office.agent(owner, f"{backend} managed worker", "worker")
        _, tokens[backend] = register_runner(office, owner, f"{backend} inference runner")
        assert configure(office, owner, 50).get("budget")
        prepared[backend] = (owner, lead, agent, image)
    host.setup(tokens)
    runs = []
    for backend, (owner, lead, agent, image) in prepared.items():
        runtime, config, state = install_backend(office, host, owner, backend, image, endpoint)
        marker = f"office-{uuid4().hex[:12]}"
        task = office.tool(lead, "office_create_task",
                           objective=f"Call the mandatory CoCodeOp MCP context tool first, then reply with exactly {marker}.",
                           acceptance_criteria=["One successful actual MCP context call and the exact marker"],
                           idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                            idempotency_key=str(uuid4()))["task"]
        job = enqueue(office, lead, ready, agent, runtime)["job"]
        report = invoke(backend, host, service, forwards, config, state)
        run, evidence, events = get_attempt(host, state, report)
        assert run["lease"]["job_id"] == job["job_id"]
        assert run["executor"]["agent_id"] == agent["agent_id"]
        assert evidence["backend"] == BACKENDS[backend][0] and evidence["model"] == GATEWAY_MODEL
        assert marker in events[-1]["summary"], events[-1]
        stored = office.tool(lead, "office_get_task", task_id=task["task_id"])
        assert marker in json.dumps(stored) and key not in json.dumps(stored)
        assert stored["runs"][0]["backend"] == evidence
        runs.append({
            "backend": evidence["backend"], "native_version": BACKENDS[backend][2], "wire_api": BACKENDS[backend][1],
            "image": image, "owner": run["executor"]["user_id"], "agent": run["executor"]["agent_id"],
            "run_id": run["run_id"], "task_id": task["task_id"], "outcome": run["status"],
            "successful_mcp_call": "office_run_context", "evidence": evidence,
        })
    assert len({run["evidence"]["session_id"] for run in runs}) == len(runs)
    if len(runs) == 2:
        assert len({run["owner"] for run in runs}) == 2
    return {
        "provider": IMAGE, "model": GATEWAY_MODEL, "runs": runs,
        "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--codex-image")
    parser.add_argument("--opencode-image")
    args = parser.parse_args()
    images = {backend: image for backend, image in
              (("codex", args.codex_image), ("opencode", args.opencode_image)) if image}
    assert images, "Supply at least one local image ID"
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    runner = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-native-agents-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "native-agents") as office, gateway() as (url, key):
            host = RunnerHost(root, args.linux_vm, runner)
            try:
                print(json.dumps(verify(office, host, images, url, key)))
            finally:
                host.close()
