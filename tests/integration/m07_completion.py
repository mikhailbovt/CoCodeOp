"""Verify fenced result publication using actual Linux fixture program exits."""

import argparse
import hashlib
import json
import os
import tempfile
from pathlib import Path
from uuid import uuid4

from m05a_registry import RunnerHost
from m07_leases import RunnerControl, register_runner
from m07_queue import configure, create_template, enqueue
from m07_start_effects import proof
from workspace_fixture import ROOT, workspace_fixture


EXECUTE = r"""
import json, subprocess, sys
code = int(sys.argv[1])
command = [sys.executable, '-c', 'print(\'{"result":"checked"}\'); raise SystemExit(' + str(code) + ')']
process = subprocess.Popen(command, stdout=subprocess.PIPE, text=True)
output, _ = process.communicate(timeout=5)
print(json.dumps({'external_id': f'fixture:{process.pid}:{sys.argv[2]}', 'stdout': output,
                  'exit_code': process.returncode}))
"""


def verify(office, tmp, guest, binary):
    lead, _ = office.agent(office.owner_a, "Completion lead", "lead")
    worker, worker_id = office.agent(office.owner_a, "Completion worker", "worker")
    reviewer, reviewer_id = office.agent(office.owner_b, "Peer review agent", "reviewer")
    runtime = create_template(office, office.owner_a, units=10)
    assert configure(office, office.owner_a, 60).get("budget")
    _, runner_token = register_runner(office, office.owner_a, "Result publisher")
    _, other = register_runner(office, office.owner_a, "Standby controller")
    host = RunnerHost(tmp, guest, binary)
    host.setup({"runner": runner_token, "other": other})
    ctl = RunnerControl(office, host)

    def recipient(identity):
        return {"user_id": identity["owner_user_id"], "agent_id": identity["agent_id"]}

    def launch(exit_code):
        draft = office.tool(lead, "office_create_task", objective="Capture the actual process exit and output",
                            acceptance_criteria=["Verify the process produced the expected JSON"],
                            visibility={"kind": "explicit_acl"},
                            grants=[{**recipient(identity), "right": "read"} for identity in (worker_id, reviewer_id)],
                            review_policy={"kind": "explicit_reviewer", "reviewer": recipient(reviewer_id)},
                            idempotency_key=str(uuid4()))["task"]
        ready = office.tool(lead, "office_prepare_task", task_id=draft["task_id"], expected_revision=draft["revision"],
                            idempotency_key=str(uuid4()))["task"]
        job = enqueue(office, lead, ready, worker_id, runtime)["job"]
        run = ctl.claim("runner")["run"]
        assert run["lease"]["job_id"] == job["job_id"]
        permit = ctl.send("runner", "authorize-start", {"lease": proof(run), "idempotency_key": str(uuid4())})
        external = json.loads(host.invoke(["python3", "-c", EXECUTE, str(exit_code), permit["effect"]["effect_id"]],
                                          check=True).stdout)
        effect = {"lease": proof(run), "effect_id": permit["effect"]["effect_id"]}
        started = ctl.send("runner", "observe-start", {"effect": effect, "external_id": external["external_id"]})
        assert started["run"]["status"] == "running" and external["exit_code"] == exit_code
        return job, {"effect": effect, "external_id": external["external_id"], "exit_code": external["exit_code"],
                     "idempotency_key": str(uuid4())}, external["stdout"]

    def task(job, token=reviewer):
        return office.tool(token, "office_get_task", task_id=job["task_id"])

    def result(job):
        return office.tool(reviewer, "office_get_task_result", task_id=job["task_id"])

    def budget():
        current = office.tool(office.owner_a, "office_get_execution_budget")
        return current["reserved_units"], current["charged_units"]

    try:
        ctl.advertise("runner", 1, [runtime])
        ctl.advertise("other", 1, [runtime])
        job, payload, output = launch(0)
        report = {"artifacts": [{"name": "receipt.json", "format": "json", "content": output}],
                  "agent_reported_checks": [{"name": "Bounded artifact output", "outcome": "passed",
                                             "notes": "a\u001f" * 490} for _ in range(8)]}
        for number, check in enumerate(report["agent_reported_checks"]):
            check["name"] += f" {number + 1}"
        payload["result"] = {"kind": "succeeded", "summary": "Published the authentic process output", "report": report}
        assert ctl.send("other", "complete", payload, check=False).returncode != 0
        stale = {**payload, "effect": {**payload["effect"], "lease": {**payload["effect"]["lease"], "run_fence": 1_000_000}}}
        assert "RunLeaseStale" in ctl.send("runner", "complete", stale, check=False).stderr
        assert ctl.send("runner", "complete", {**payload, "exit_code": 7}, check=False).returncode != 0
        broken = {"artifacts": [{"name": "receipt.json", "format": "json", "content": "{broken"}]}
        assert ctl.send("runner", "complete", {**payload, "result": {**payload["result"], "report": broken}},
                        check=False).returncode != 0
        assert task(job)["runs"][0]["status"] == "running" and result(job)["code"] == "INVALID_TRANSITION"
        assert budget() == (10, 0)
        checks = ["wrong_runner_fence_exit_or_invalid_artifact_cannot_publish_or_charge"]

        completed = ctl.send("runner", "complete", payload)
        run = completed["execution"]["run"]
        assert run["status"] == "succeeded" and completed["exit_code"] == 0
        assert completed["execution"]["effect"]["state"] == "stopped"
        snapshot = result(job)
        assert snapshot["run_id"] == run["run_id"] and snapshot["author"] == recipient(worker_id)
        assert snapshot["content"]["artifacts"][0]["content"] == output
        assert snapshot["content"]["artifacts"][0]["content_sha256"] == hashlib.sha256(output.encode()).hexdigest()
        assert snapshot["content"]["agent_reported_checks"] == report["agent_reported_checks"]
        assert task(job)["task"]["status"] == "in_review" and budget() == (0, 10)
        assert office.tool(lead, "office_get_execution", job_id=job["job_id"])["state"] == "completed"
        review = {"task_id": job["task_id"], "expected_revision": completed["task_revision"], "run_id": run["run_id"],
                  "decision": "accept", "reason": "Compared the exact artifact and the criteria"}
        assert office.tool(reviewer, "office_review_task", **review, idempotency_key=str(uuid4()))["code"] == "INVALID_INPUT"
        reviewed = office.tool(reviewer, "office_review_task", **review, binding=snapshot["binding"], idempotency_key=str(uuid4()))
        assert reviewed["task"]["status"] == "done"
        office.restart()
        assert ctl.send("runner", "complete", payload) == completed and budget() == (0, 10)
        changed = {**payload, "result": {**payload["result"], "summary": "Changed after review"}}
        assert "IdempotencyConflict" in ctl.send("runner", "complete", changed, check=False).stderr
        assert office.sql(f"SELECT count(*) FROM task_result_revisions WHERE task_id='{job['task_id']}'") == "1"
        checks.append("confirmed_process_result_is_reviewed_by_peer_and_replays_after_restart")

        failed, payload, _ = launch(7)
        payload["result"] = {"kind": "failed", "reason": "Fixture program exited with status 7"}
        receipt = ctl.send("runner", "complete", payload)
        assert receipt["exit_code"] == 7 and receipt["execution"]["run"]["status"] == "failed"
        assert task(failed)["task"]["status"] == "failed" and result(failed)["code"] == "INVALID_TRANSITION"
        dead = office.tool(lead, "office_get_execution", job_id=failed["job_id"])
        assert dead["state"] == "dead_letter" and dead["terminal_reason"] == "execution_failed"
        assert budget() == (0, 20)
        checks.append("nonzero_process_exit_is_persisted_and_charged_without_a_success_artifact")

        expired, late, _ = launch(0)
        run_id = late["effect"]["lease"]["run_id"]
        office.sql(f"UPDATE runs SET lease_until=clock_timestamp()-interval '1 second' WHERE id='{run_id}'")
        late["result"] = {"kind": "succeeded", "summary": "Too late to publish"}
        assert "RunLeaseStale" in ctl.send("runner", "complete", late, check=False).stderr
        assert ctl.claim("other") is None
        assert task(expired)["runs"][0]["status"] == "unknown" and result(expired)["code"] == "INVALID_TRANSITION"
        assert budget() == (10, 20)
        ctl.send("runner", "reconcile-start", {"effect": late["effect"], "observation": {
            "kind": "stopped", "external_id": late["external_id"],
        }})
        assert budget() == (0, 30) and result(expired)["code"] == "INVALID_TRANSITION"
        checks.append("expired_confirmed_run_blocks_publication_and_reconciles_only_the_observed_exit")
        return checks
    finally:
        host.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux-vm", action="store_true")
    parser.add_argument("--runner", type=Path)
    args = parser.parse_args()
    assert args.linux_vm or os.name != "nt", "Use --linux-vm on Windows"
    binary = (args.runner or ROOT / (".cocodeop-test/linux-bin/office-runner" if args.linux_vm
                                     else "target/release/office-runner")).resolve()
    assert binary.is_file(), f"Build the Linux runner: {binary}"
    with tempfile.TemporaryDirectory(prefix="cocodeop-completion-") as directory:
        tmp = Path(directory)
        with workspace_fixture(tmp, "scheduler-completion") as office:
            print(json.dumps({"checks": verify(office, tmp, args.linux_vm, binary),
                              "linux_execution": "debian13_qemu_guest" if args.linux_vm else "native_linux",
                              "independent_physical_hosts": False}))
