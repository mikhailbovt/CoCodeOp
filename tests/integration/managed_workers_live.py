"""Exercise lead-owned worker creation, status, and a real backend's standing instructions."""

import argparse
import contextlib
import http.client
import http.server
import json
import os
import tempfile
import threading
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from urllib.parse import urlsplit
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from m09_native_agents import GATEWAY_MODEL, get_attempt, install_backend, invoke
from m09_native_checkout import inference_provider
from m10_activation import wait_for
from freellmapi_live import MODEL, MODEL_DIGEST
from workspace_fixture import ROOT, workspace_fixture


@contextlib.contextmanager
def observe_system_input(url, marker):
    target = urlsplit(url)
    assert target.scheme == "http" and target.hostname in ("localhost", "127.0.0.1")
    seen = threading.Event()

    class Forward(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            size = int(self.headers.get("Content-Length", "0"))
            if not 1 <= size <= 2 * 1024 * 1024:
                self.send_error(413)
                return
            body = self.rfile.read(size)
            value = json.loads(body)
            if isinstance(value, dict):
                instruction = value.get("instructions")
                messages = value.get("messages", value.get("input", []))
                if isinstance(instruction, str) and marker in instruction:
                    seen.set()
                if isinstance(messages, list) and any(
                    isinstance(message, dict) and message.get("role") in ("system", "developer")
                    and marker in json.dumps(message.get("content"))
                    for message in messages
                ):
                    seen.set()
            proxy = http.client.HTTPConnection(target.hostname, target.port, timeout=175)
            try:
                headers = {"Content-Type": "application/json"}
                if self.headers.get("Authorization"):
                    headers["Authorization"] = self.headers["Authorization"]
                proxy.request("POST", self.path, body=body, headers=headers)
                response = proxy.getresponse()
                self.send_response(response.status)
                self.send_header("Content-Type", response.getheader("Content-Type", "application/json"))
                self.end_headers()
                with contextlib.suppress(ConnectionError):
                    while data := response.read1(65536):
                        self.wfile.write(data)
                        self.wfile.flush()
            finally:
                proxy.close()

        def log_message(self, *_):
            pass

    endpoint = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Forward)
    process = threading.Thread(target=endpoint.serve_forever, daemon=True)
    process.start()
    try:
        yield f"http://127.0.0.1:{endpoint.server_port}", seen
    finally:
        endpoint.shutdown()
        process.join(5)
        endpoint.server_close()


def verify(office, host, backend, image, url, provider_key, model):
    owner = office.owner_b
    lead, lead_info = office.agent(owner, "Team lead Bob reports to", "lead")
    other_lead, _ = office.agent(owner, "Second lead", "lead")
    remote_lead, _ = office.agent(office.owner_a, "Other owner lead", "lead")
    marker = "bob-profile-" + uuid4().hex[:12]
    input = {"display_name": "Backend Bob", "instructions": f"End your final task summary with the exact reference {marker}.",
             "idempotency_key": str(uuid4())}
    assert office.tool(owner, "office_create_managed_worker", **input)["code"] == "FORBIDDEN"
    bob = office.tool(lead, "office_create_managed_worker", **input)
    assert bob.get("profile", {}).get("revision") == 1, bob
    assert bob["lead"]["agent_id"] == lead_info["agent_id"] and bob["worker"]["user_id"] == lead_info["owner_user_id"]
    assert office.tool(lead, "office_create_managed_worker", **input) == bob
    worker = bob["worker"]
    selector = {"by": "name", "display_name": "backend bob"}
    assert office.tool(other_lead, "office_worker_status", worker=selector)["code"] == "FORBIDDEN"
    assert office.tool(remote_lead, "office_worker_status", worker={"by": "id", "agent_id": worker["agent_id"]})["code"] == "FORBIDDEN"
    assert office.tool(lead, "office_worker_status", worker=selector) == {"worker": bob, "tasks": [], "next_tasks_after": None}
    assert office.sql(f"SELECT count(*) FROM credentials WHERE agent_id='{worker['agent_id']}'") == "0"
    team = office.tool(lead, "office_create_team", name="Backend", idempotency_key=str(uuid4()))["team"]
    member = office.tool(lead, "office_set_team_member", team_id=team["team_id"], agent_id=worker["agent_id"], role="worker",
                         expected_revision=team["revision"], idempotency_key=str(uuid4()))
    assert member.get("member", {}).get("member") == worker, member

    _, token = register_runner(office, owner, "Backend worker process host")
    secrets = {backend: token}
    if provider_key:
        secrets["provider"] = provider_key
    host.setup(secrets)
    assert configure(office, owner, 65).get("budget")
    with observe_system_input(url, marker) as (traced, sent_with_role):
        core_forward, address = host.connection_for_runner(office.endpoint)
        provider_forward, provider = host.connection_for_runner(urlsplit(traced).netloc)
        while provider == address:
            provider_forward, provider = host.connection_for_runner(urlsplit(traced).netloc)
        forwards = [entry for entry in (core_forward, provider_forward) if entry]
        template, config, state = install_backend(office, host, owner, backend, image, provider,
                                                  model=model, authenticated=bool(provider_key))
        task = office.tool(lead, "office_create_task", team_id=team["team_id"],
                           objective="Call office_run_context, then summarize the current task as a short verification report.",
                           acceptance_criteria=["A short verification report from the worker"],
                           idempotency_key=str(uuid4()))["task"]
        prepared = office.tool(lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                               idempotency_key=str(uuid4()))["task"]
        job = enqueue(office, lead, prepared, worker, template)["job"]
        queued = office.tool(lead, "office_worker_status", worker=selector)
        assert queued["tasks"][0]["job"]["job_id"] == job["job_id"] and queued["tasks"][0]["job"]["state"] == "queued", queued
        with ThreadPoolExecutor(max_workers=1) as pool:
            result = pool.submit(invoke, backend, host, address, forwards, config, state)
            wait_for(lambda: office.sql(f"SELECT profile_revision FROM managed_worker_run_profiles WHERE task_id='{task['task_id']}'") == "1", 35)
            update = {"agent_id": worker["agent_id"], "expected_revision": 1, "idempotency_key": str(uuid4()),
                      "instructions": "Use a different marker in future reports."}
            assert office.tool(other_lead, "office_update_managed_worker", **update)["code"] == "FORBIDDEN"
            updated = office.tool(lead, "office_update_managed_worker", **update)
            assert updated.get("profile", {}).get("revision") == 2, updated
            assert office.tool(lead, "office_update_managed_worker", **update) == updated
            assert office.tool(lead, "office_update_managed_worker", **dict(update, idempotency_key=str(uuid4())))["code"] == "REVISION_CONFLICT"
            run, evidence, _ = get_attempt(host, state, result.result(timeout=420))
        assert sent_with_role.is_set(), "The inference provider did not receive the standing instructions in a system or developer message"
        assert marker in run["summary"] and run["executor"] == worker, run
        status = office.tool(lead, "office_worker_status", worker=selector)
        assert status["worker"]["profile"]["revision"] == 2
        assert status["tasks"][0]["job"]["state"] == "completed"
        assert status["tasks"][0]["state"]["runs"][0] == run
        assert office.cli(lead, "workers", "status", "--workspace", office.workspace, "--name", "Backend Bob") == status

    return {"worker": worker, "lead": bob["lead"], "team_id": team["team_id"], "run_id": run["run_id"],
            "backend": evidence, "display_name": bob["profile"]["display_name"], "running_profile_revision": 1,
            "latest_profile_revision": 2,
            "checks": ["lead_creates_own_worker_without_an_interactive_credential", "lead_adds_worker_to_own_team",
                       "other_owner_and_sibling_lead_cannot_inspect_or_modify_worker",
                       "worker_status_shows_queued_job_and_real_result_by_name", "standing_instructions_reach_the_native_model",
                       "active_attempt_keeps_its_reviewed_instruction_revision", "cli_and_mcp_return_the_same_status"]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--backend", choices=("codex", "opencode"), required=True)
    parser.add_argument("--worker-image-id", required=True)
    parser.add_argument("--ollama-model", default=MODEL)
    parser.add_argument("--ollama-digest", default=MODEL_DIGEST)
    parser.add_argument("--model-alias", default=GATEWAY_MODEL)
    parser.add_argument("--ollama-direct", action="store_true")
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-worker-profile-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "managed-worker") as office, inference_provider(args) as (url, key, model, provider):
            host = RunnerHost(root, args.linux_vm, binary)
            try:
                report = verify(office, host, args.backend, args.worker_image_id, url, key, model)
                report.update(provider=provider, model=model, image_id=args.worker_image_id,
                              schema=int(office.sql("SELECT max(version) FROM _sqlx_migrations")))
                print(json.dumps(report))
            except Exception:
                import sys
                print(office.compose("logs", "--no-color", "--tail", "15", "db").stdout[-4600:], file=sys.stderr)
                raise
            finally:
                host.close()
