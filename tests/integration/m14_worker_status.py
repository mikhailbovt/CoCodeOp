"""Use real lead clients to inspect their own named workers after a Core restart."""

import argparse
import json
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_queue import configure, create_template, enqueue
from m09_native_checkout import inference_provider
from m14_acceptance import Lead
from m14_project import completed_results
from workspace_fixture import ROOT, workspace_fixture


def prepare(office, owner, host, name, provider, model):
    credential = office.agent(owner, f"{name}'s lead", "lead")
    lead = Lead(host, credential, office, provider, model)
    worker = office.tool(lead.token, "office_create_managed_worker", display_name=name,
                         instructions="Work on the tasks assigned to you and report your results.",
                         idempotency_key=str(uuid4()))["worker"]
    team = office.tool(lead.token, "office_create_team", name=f"{name}'s team",
                       idempotency_key=str(uuid4()))["team"]
    office.tool(lead.token, "office_set_team_member", team_id=team["team_id"], expected_revision=team["revision"],
                agent_id=worker["agent_id"], role="worker", idempotency_key=str(uuid4()))
    assert configure(office, owner, 100).get("budget")
    template = create_template(office, owner, units=10)
    task = office.tool(lead.token, "office_create_task", team_id=team["team_id"],
                       objective="Check the API contract", acceptance_criteria=["Report the result"],
                       idempotency_key=str(uuid4()))["task"]
    ready = office.tool(lead.token, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                        idempotency_key=str(uuid4()))["task"]
    job = enqueue(office, lead.token, ready, worker, template)["job"]
    return lead, name, worker, job


def ask_status(office, lead, name, worker, job):
    selector = {"by": "name", "display_name": name}
    arguments = {"workspace_id": office.workspace, "worker": selector}
    prompts = (
        f"How is {name} doing? Verify this in CoCodeOp. Use office_office_bootstrap to confirm the workspace, "
        "then read the worker status by name through office_office_worker_status before answering.",
        f"Invoke office_office_worker_status with {json.dumps(arguments)}. Answer with the real task and queue state.",
        f"Read {name}'s status using the actual office_office_worker_status function. "
        + "\n".join(f"Parameter {key} = {json.dumps(value)}" for key, value in arguments.items()),
    )
    for prompt in prompts:
        turn = lead.turn("named_worker_after_restart", prompt, "office_bootstrap", "office_worker_status")
        for status in completed_results(turn, "office_worker_status"):
            assert status["worker"]["worker"] == worker, status
            result = next(entry for entry in status["tasks"] if entry["job"]["job_id"] == job["job_id"])
            assert result["job"]["state"] == "queued" and result["state"]["task"]["task_id"] == job["task_id"], result
            return {"owner_user_id": lead.identity["owner_user_id"], "lead_agent_id": lead.identity["agent_id"],
                    "worker_agent_id": worker["agent_id"], "display_name": name, "client": turn["client"],
                    "version": turn["version"], "session_id": turn["session_id"], "task_id": job["task_id"],
                    "job_id": job["job_id"], "job_state": result["job"]["state"], "tool": "office_worker_status",
                    "machine_id_sha256": turn["machine_id_sha256"]}
    raise AssertionError(("No lead MCP status call was observed", name, lead.turns))


def verify(office, hosts, provider, model):
    queries = [prepare(office, owner, host, name, provider, model) for owner, host, name in
               zip((office.owner_a, office.owner_b), hosts, ("Backend Bob", "Client Charlie"))]
    previous = office.endpoint
    office.restart()
    with ThreadPoolExecutor(max_workers=2) as pool:
        pending = [pool.submit(ask_status, office, *query) for query in queries]
        return {"schema": int(office.sql("SELECT max(version) FROM _sqlx_migrations")),
                "core_endpoint_changed": previous != office.endpoint, "workers": [result.result() for result in pending]}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--vm-a", type=Path, default=ROOT / ".cocodeop-test/linux-vm-a")
    parser.add_argument("--vm-b", type=Path, default=ROOT / ".cocodeop-test/linux-vm")
    parser.add_argument("--runner", type=Path, required=True)
    parser.add_argument("--server-image", required=True)
    parser.add_argument("--ollama-url", default="http://127.0.0.1:11434")
    parser.add_argument("--ollama-model", required=True)
    parser.add_argument("--ollama-digest", required=True)
    args = parser.parse_args()
    args.ollama_direct = True
    with tempfile.TemporaryDirectory(prefix="cocodeop-m14-worker-status-") as directory:
        root = Path(directory)
        with (workspace_fixture(root, "worker-status", server_image=args.server_image) as office,
              inference_provider(args) as (url, _, model, _)):
            hosts = []
            try:
                for index, vm in enumerate((args.vm_a, args.vm_b)):
                    guest = root / f"guest-{index}"
                    guest.mkdir()
                    host = RunnerHost(guest, True, args.runner, vm_state=vm)
                    hosts.append(host)
                    host.setup({})
                print(json.dumps(verify(office, hosts, url, model)))
            finally:
                for host in hosts:
                    host.close()
