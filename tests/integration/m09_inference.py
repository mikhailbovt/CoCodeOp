"""Exercise the installed model profile with real FreeLLMAPI and MCP inside a managed worker."""

import argparse
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


WORKER = r"""
import json, os, socket, subprocess, time
from pathlib import Path
from urllib.error import HTTPError
from urllib.request import Request, urlopen
app = "/usr/local/bin/office-runner"
model = os.environ["COCODEOP_MODEL"]
base = "http://127.0.0.1:8417/v1/"
assert os.environ["COCODEOP_INFERENCE_API"] == "responses"
assert "OPENAI_API_KEY" not in os.environ and "OFFICE_TOKEN" not in os.environ
assert not any("freellmapi-" in value for value in os.environ.values())
assert Path(os.environ["COCODEOP_INFERENCE_SOCKET"]).is_socket()
assert len(Path("/proc/net/route").read_text().splitlines()) == 1
def fetch(route, data=None, reject=None):
    request = Request(base + route, data=None if data is None else json.dumps(data).encode(),
                      headers={"Content-Type": "application/json"} if data is not None else {})
    try:
        with urlopen(request, timeout=125) as response:
            assert reject is None
            if response.headers.get_content_type() == "text/event-stream":
                messages = []
                for raw in response:
                    if raw.startswith(b"data:"):
                        data = raw[5:].strip()
                        if data and data != b"[DONE]":
                            messages.append(json.loads(data))
                return messages
            return json.load(response)
    except HTTPError as error:
        assert error.code == reject, (route, error.code)
def run_context():
    request = {"jsonrpc":"2.0","id":1,"method":"tools/call",
               "params":{"name":"office_run_context","arguments":{}}}
    result = subprocess.run([app, "worker-mcp"], input=json.dumps(request)+"\n",
                            text=True, check=True, capture_output=True, timeout=15)
    return json.loads(result.stdout)["result"]["structuredContent"]
with subprocess.Popen([app, "worker-inference"]) as bridge:
    try:
        for _ in range(50):
            try:
                with socket.create_connection(("127.0.0.1", 8417), timeout=.3):
                    break
            except OSError:
                if bridge.poll() is not None:
                    raise AssertionError("The inference bridge stopped")
                time.sleep(.1)
        inventory = fetch("models")["data"]
        assert len(inventory) == 1 and inventory[0]["id"] == model
        fetch("responses", {"model":"different-model","input":"test"}, reject=400)
        fetch("chat/completions", {"model":model,"messages":[]}, reject=404)
        fetch("responses", {"model":model,"input":"test","max_output_tokens":20000}, reject=400)
        tool = {"type":"function","name":"office_run_context",
                "description":"Get the assigned task and its private receipt marker.",
                "parameters":{"type":"object","properties":{},"additionalProperties":False}}
        user = {"role":"user","content":"Call office_run_context exactly once with {}. After the tool result, "
                "reply with the exact marker from the task objective."}
        body = {"model":model,"tools":[tool],"temperature":0,"input":[user],"stream":True,
                "max_output_tokens":1400,"store":False}
        streamed = fetch("responses", body)
        calls = [event["item"] for event in streamed if event.get("type") == "response.output_item.done"
                 and event.get("item", {}).get("type") == "function_call"]
        if not calls:
            response = next(event["response"] for event in streamed if event.get("type") == "response.completed")
            calls = [entry for entry in response["output"] if entry.get("type") == "function_call"]
        assert len(calls) == 1 and calls[0]["name"] == "office_run_context"
        assert not json.loads(calls[0]["arguments"])
        context = run_context()
        assert context["run_id"] and context["executor"]["agent_id"]
        marker = context["task"]["objective"].split()[-1]
        call = {key:calls[0][key] for key in ("type","name","arguments","call_id")}
        tool_result = {"type":"function_call_output","call_id":call["call_id"],"output":json.dumps(context)}
        reply = fetch("responses", dict(body, input=[user,call,tool_result], stream=False))
        assert any(marker in piece.get("text","") for entry in reply["output"] if entry.get("type") == "message"
                   for piece in entry.get("content",[])), "The model did not return the actual task marker"
        fetch("models", reject=429)
        Path(os.environ["COCODEOP_RESULT"]).write_text(json.dumps({
            "kind":"succeeded","summary":"Verified real inference and task receipt " + marker
        }))
    finally:
        bridge.terminate()
"""


def verify(office, host, image, gateway_url, gateway_key):
    lead, _ = office.agent(office.owner_b, "Inference test lead", "lead")
    _, agent = office.agent(office.owner_b, "Inference test worker", "worker")
    _, runner_token = register_runner(office, office.owner_b, "Inference profile runner")
    host.setup({"runner": runner_token, "provider": gateway_key})
    assert configure(office, office.owner_b, 30).get("budget")
    coordinator_forward, service = host.connection_for_runner(office.endpoint)
    provider_forward, endpoint = host.connection_for_runner(urlsplit(gateway_url).netloc)
    while endpoint == service:
        provider_forward, endpoint = host.connection_for_runner(urlsplit(gateway_url).netloc)
    profile = {"name": "freellm-local.responses", "base_url": f"http://{endpoint}/v1",
               "model": GATEWAY_MODEL, "api": "responses", "max_requests": 3,
               "max_output_tokens": 1400, "timeout_seconds": 125}
    host.replace("inference-profile.json", json.dumps(profile))
    file = f"{host.root}/inference-profile.json"
    host.invoke(["chmod", "600", file], check=True)
    digest = host.invoke([host.installed, "inference-profile", "--file", file, "--json"], check=True)
    described = json.loads(digest.stdout)
    assert described["model"] == GATEWAY_MODEL and "base_url" not in described
    manifest = {"adapter_id": "live-inference-check.v1", "image_id": image, "entrypoint": "/usr/local/bin/python3",
                "args": ["-c", WORKER], "timeout_seconds": 280, "memory_mib": 128, "cpus": 1, "pids_limit": 64,
                "run_mcp": True, "provider_profile_sha256": described["provider_profile_sha256"]}
    host.replace("inference-runtime.json", json.dumps(manifest))
    file = f"{host.root}/inference-runtime.json"
    host.invoke(["chmod", "600", file], check=True)
    approved = host.invoke([host.installed, "runtime-manifest", "--file", file, "--json"], check=True)
    runtime = office.tool(office.owner_b, "office_create_runtime_template", name="FreeLLM managed inference", kind="managed",
                          managed={"adapter_id": manifest["adapter_id"], "reservation_units": 15,
                                   "manifest_sha256": json.loads(approved.stdout)["manifest_sha256"]},
                          idempotency_key=str(uuid4()))["template_id"]
    state = f"{host.root}/runner"
    host.invoke(["mkdir", "-m", "700", state], check=True)
    configuration = {"capabilities": ["linux"], "runtimes": [{"template_id": runtime, "manifest": manifest}],
                     "providers": [{"profile": profile, "bearer_file": f"{host.root}/provider.token"}]}
    host.replace("profile.json", json.dumps(configuration))
    config = f"{host.root}/profile.json"
    host.invoke(["chmod", "600", config], check=True)
    marker = f"receipt-{uuid4().hex[:14]}"
    task = office.tool(lead, "office_create_task", objective=f"Return the actual result marker {marker}",
                       acceptance_criteria=["Read the assigned task through live MCP"], idempotency_key=str(uuid4()))["task"]
    ready = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                        idempotency_key=str(uuid4()))["task"]
    job = enqueue(office, lead, ready, agent, runtime)["job"]
    forwards = [route for route in (coordinator_forward, provider_forward) if route]
    result = host.invoke([host.installed, "serve", "--file", config, "--state-dir", state,
                          "--token-file", f"{host.root}/runner.token", "--url", f"http://{service}",
                          "--once", "--json"], forward=forwards, timeout=310)
    assert result.returncode == 0, (result.stdout, result.stderr)
    report = json.loads(result.stdout)
    execution = report["receipt"]["execution"]["run"]
    assert execution["status"] == "succeeded" and execution["lease"]["job_id"] == job["job_id"], report
    view = office.tool(lead, "office_get_task", task_id=task["task_id"])
    assert marker in json.dumps(view) and gateway_key not in json.dumps(view)
    return {
        "checks": ["real_freellm_responses_stream_roundtrips_an_actual_mcp_result_in_a_managed_worker",
                   "provider_model_route_limits_and_credentials_are_enforced_by_the_host_socket"],
        "provider": IMAGE,
        "model": GATEWAY_MODEL,
        "agent_backend": "protocol test worker",
        "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux",
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--image-id", required=True)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-inference-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "model-profile") as office, gateway() as (url, key):
            host = RunnerHost(root, args.linux_vm, binary)
            try:
                print(json.dumps(verify(office, host, args.image_id, url, key)))
            finally:
                host.close()
