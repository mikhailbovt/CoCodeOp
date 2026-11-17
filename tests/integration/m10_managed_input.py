"""Run a real isolated bridge process that pauses, releases a claim, and continues after input."""

import argparse
import hashlib
import json
import os
import sys
import tempfile
from pathlib import Path
from uuid import uuid4

from m05a_claims import Participant, file, recipient
from m05a_registry import MANIFEST, RunnerHost
from m07_leases import register_runner
from m07_queue import configure, enqueue
from m09_generic_bridge import install_bridge
from m09_native_agents import get_attempt
from m09_native_checkout import checkout_service, enqueue_task
from workspace_fixture import ROOT, workspace_fixture


def verify(office, host, image):
    lead = office.agent(office.owner_b, "Managed task lead", "lead")
    checkout_runner, checkout_token = register_runner(office, office.owner_a, "Source authority")
    runner, runner_token = register_runner(office, office.owner_b, "Managed bridge host")
    host.setup({"checkout": checkout_token, "runner": runner_token})
    root = f"{host.root}/tree/code"
    host.invoke(["chmod", "755", "--", root, f"{root}/src"], check=True)
    host.invoke(["chmod", "644", "--", f"{root}/src/app.rs"], check=True)
    project = office.tool(office.owner_a, "office_create_project", name="Paused solution",
                          visibility={"kind": "workspace"}, idempotency_key=str(uuid4()))["project"]
    requested = office.tool(office.owner_a, "office_request_checkout", project_id=project["project_id"],
                            host_runner_id=checkout_runner, manifest=MANIFEST, idempotency_key=str(uuid4()))["checkout"]
    enrolled = host.enroll(office.endpoint, requested, "checkout", "tree/code")
    assert enrolled.returncode == 0, enrolled.stderr
    checkout = office.tool(office.owner_a, "office_get_checkout", checkout_id=requested["checkout_id"])
    paths = [file("src/app.rs")]
    actor = Participant(office, checkout, lead, office.owner_b, "Paused worker",
                        {"read": {"allow": paths}, "write": {"allow": paths}, "write_operations": ["replace"]},
                        objective="Ask for direction after claiming the permitted path")
    assert configure(office, office.owner_b, 75).get("budget")
    runtime, config, state = install_bridge(office, host, office.owner_b, image, checkout=True)

    def cycle():
        forward, endpoint = host.connection_for_runner(office.endpoint)
        result = host.invoke([host.installed, "serve", "--file", config, "--state-dir", state,
                              "--url", f"http://{endpoint}", "--token-file", f"{host.root}/runner.token",
                              "--once", "--json"], forward=forward, timeout=78, check=True)
        return json.loads(result.stdout)

    def paused_run(job):
        result = cycle()
        assert result["status"] == "completed", result
        run = result["receipt"]["execution"]["run"]
        if run["status"] != "paused":
            diagnostic = host.invoke(["tail", "-c", "2200", f"{state}/run-{run['run_id']}/scratch/agent/native.stderr"]).stdout
            raise AssertionError((result, diagnostic))
        assert run["lease"]["job_id"] == job["job_id"] and run["backend"]["terminal_state"] == "input_required"
        assert run["backend"]["exit_code"] == 0 and run["ended_at"]
        checkpoint_path = f"{state}/run-{run['run_id']}/scratch/checkpoint.json"
        checkpoint = host.invoke(["cat", checkpoint_path], check=True).stdout.encode()
        assert run["backend"]["checkpoint_id"] == "sha256:" + hashlib.sha256(checkpoint).hexdigest()
        assert office.tool(office.owner_b, "office_get_execution_budget")["reserved_units"] == 0
        assert office.tool(actor.lead, "office_get_execution", job_id=job["job_id"])["state"] == "waiting_input"
        return run

    def coordination(objective):
        task = office.tool(actor.lead, "office_create_task", objective=objective, acceptance_criteria=["Use the run's exact input"],
                           idempotency_key=str(uuid4()))["task"]
        ready = office.tool(actor.lead, "office_prepare_task", task_id=task["task_id"], expected_revision=task["revision"],
                            idempotency_key=str(uuid4()))["task"]
        return task, enqueue(office, actor.lead, ready, actor.identity, runtime)["job"]

    with checkout_service(host, office, checkout):
        job = enqueue_task(office, actor, runtime)
        paused = paused_run(job)
        claims = actor.history()["claims"]
        assert len(claims) == 1 and claims[0]["state"] == "fenced" and claims[0]["run_id"] == paused["run_id"], claims
        assert office.sql(f"SELECT state FROM execution_start_effects WHERE run_id='{paused['run_id']}'") == "stopped"

        other, _ = coordination("Verify the assigned MCP run identity")
        independent, _, _ = get_attempt(host, state, cycle())
        assert independent["task_id"] == other["task_id"] and independent["lease"]["runner_id"] == runner

        waiting = {"task_id": actor.task, "run_id": paused["run_id"]}
        view = office.tool(actor.lead, "office_get_managed_input", **waiting)
        assert view["state"] == "waiting" and view["checkpoint_id"] == paused["backend"]["checkpoint_id"]
        assert office.tool(office.owner_a, "office_get_managed_input", **waiting)["code"] == "FORBIDDEN"
        assert office.cli(actor.lead, "scheduler", "input", "--workspace", office.workspace,
                          "--task", actor.task, "--run", paused["run_id"]) == view
        # The checkout coordinator maintains a lease. Restart coverage for the wait is exercised
        # below with the separate coordination task so the source host can finish this one.
        current = office.tool(actor.lead, "office_get_task", task_id=actor.task)["task"]
        assert current["status"] == "blocked"
        marker = "approved-" + uuid4().hex[:12]
        reply = {**waiting, "expected_task_revision": current["revision"], "idempotency_key": str(uuid4()), "answer": marker}
        assert office.tool(actor.worker, "office_answer_managed_input", **reply)["code"] == "FORBIDDEN"
        advanced = office.tool(actor.lead, "office_answer_managed_input", **reply)
        assert advanced["job"]["state"] == "queued" and advanced["input"]["state"] == "answered", advanced
        assert office.tool(actor.lead, "office_answer_managed_input", **reply) == advanced
        continued, _, _ = get_attempt(host, state, cycle())
        assert continued["task_id"] == actor.task and continued["attempt_number"] == paused["attempt_number"] + 1
        assert continued["run_id"] != paused["run_id"] and continued["executor"] == recipient(actor.identity)
        assert continued["lease"]["run_fence"] > paused["lease"]["run_fence"]
        assert marker in continued["summary"] and view["checkpoint_id"] in continued["summary"]
        assert office.tool(actor.lead, "office_get_execution", job_id=job["job_id"])["state"] == "resumed"

    expiring_task, expiring_job = coordination("Ask for a value that will expire during the test")
    expiring = paused_run(expiring_job)
    office.restart()
    expired_input = {"task_id": expiring_task["task_id"], "run_id": expiring["run_id"]}
    assert office.tool(actor.lead, "office_get_managed_input", **expired_input)["state"] == "waiting"
    office.sql(f"UPDATE managed_input_waits SET expires_at=clock_timestamp()-interval '1 second' WHERE run_id='{expiring['run_id']}'")
    assert cycle()["status"] == "idle"
    outcome = office.tool(actor.lead, "office_get_execution", job_id=expiring_job["job_id"])
    assert outcome["state"] == "dead_letter" and outcome["terminal_reason"] == "input_expired"
    assert office.tool(actor.lead, "office_get_managed_input", **expired_input)["state"] == "expired"
    return {"paused": paused, "continued": continued, "expired_run": expiring["run_id"], "image": image,
            "checks": ["stopped_bridge_preserves_hashed_checkpoint", "waiting_releases_claim_runner_slot_and_budget_reservation",
                       "exact_authorized_reply_queues_fresh_fenced_attempt", "retry_uses_original_answer_receipt",
                       "wait_survives_server_restart_and_has_bounded_expiry"],
            "linux_execution": "debian13_qemu_guest" if host.guest else "native_linux"}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--image-id", required=True)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    runner = ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm else "target/release/office-runner")
    with tempfile.TemporaryDirectory(prefix="cocodeop-input-waits-") as directory:
        root = Path(directory)
        with workspace_fixture(root, "input-waits") as office:
            host = RunnerHost(root, args.linux_vm, runner)
            try:
                print(json.dumps(verify(office, host, args.image_id)))
            except Exception:
                print(office.compose("logs", "--no-color", "--tail", "16", "server", "db").stdout[-6400:], file=sys.stderr)
                raise
            finally:
                host.close()
